//! `GET /api/me/deletion` (preflight) and `DELETE /api/me` (delete the account).
//!
//! Deletion re-checks password and handle, refuses while the account still owns
//! agents or regular workspaces, and is not offered on the embedded profile (the
//! local viewer is not a server account).

use axum::{
    Json, Router,
    extract::{Extension, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use garde::Validate;
use serde::Deserialize;
use silverhand_backend_core::{
    foundation::identity::UserId,
    host_runtime::aftermath::DaemonConnectionControl,
    identity_access::{
        account_deletion::{
            AccountDeletionBlockers, AccountDeletionOutcome, AccountDeletionStore, delete_account,
        },
        auth::{AuthenticateHumanSession, SessionTokenHash},
        avatar::AvatarBlobStore,
        credential::PasswordHasher,
        user_identity::{ParticipantHandle, UserChangedSink},
    },
};

use super::human_auth::{LOGIN_PASSWORD_MIN_UTF16, password_within};
use crate::{
    foundation::{
        bearer::strict_bearer_credential, deployment_profile::DeploymentProfile,
        wire_admission::ValidatedJson,
    },
    identity_access::auth::{PublicJsonUnauthorized, require_human_session},
};

pub const ME_PATH: &str = "/api/me";
pub const PREFLIGHT_PATH: &str = "/api/me/deletion";

/// Whether this profile offers account deletion; `/api/server/info` advertises the same answer.
pub const fn offered(profile: DeploymentProfile) -> bool {
    !matches!(profile, DeploymentProfile::Embedded)
}

#[derive(Deserialize, Validate)]
struct Input {
    #[garde(custom(password_within(LOGIN_PASSWORD_MIN_UTF16)))]
    password: String,
    #[garde(skip)]
    handle: String,
}

#[derive(Clone)]
struct Deps<S, H, B, N, D> {
    store: S,
    hasher: H,
    avatars: B,
    changed: N,
    daemons: D,
    profile: DeploymentProfile,
}

pub struct AccountDeletionDeps<S, H, B, N, D> {
    pub store: S,
    pub hasher: H,
    pub avatars: B,
    pub changed: N,
    pub daemons: D,
    pub profile: DeploymentProfile,
}

pub fn router<S, H, B, N, D, A>(deps: AccountDeletionDeps<S, H, B, N, D>, sessions: A) -> Router
where
    S: AccountDeletionStore,
    H: PasswordHasher,
    B: AvatarBlobStore,
    N: UserChangedSink,
    D: DaemonConnectionControl,
    A: AuthenticateHumanSession,
{
    let deps = Deps {
        store: deps.store,
        hasher: deps.hasher,
        avatars: deps.avatars,
        changed: deps.changed,
        daemons: deps.daemons,
        profile: deps.profile,
    };
    require_human_session(
        Router::new()
            .route(PREFLIGHT_PATH, get(preflight::<S, H, B, N, D>))
            .route(ME_PATH, axum::routing::delete(delete_me::<S, H, B, N, D>))
            .with_state(deps),
        sessions,
        PublicJsonUnauthorized,
    )
}

fn blockers_json(blockers: &AccountDeletionBlockers) -> serde_json::Value {
    serde_json::json!({
        "agents": blockers.agents.iter()
            .map(|agent| serde_json::json!({"id": agent.id.as_str(), "name": agent.name}))
            .collect::<Vec<_>>(),
        "workspaces": blockers.workspaces.iter()
            .map(|ws| serde_json::json!({"id": ws.id.as_str(), "name": ws.name}))
            .collect::<Vec<_>>(),
    })
}

fn embedded_refusal() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({"error": "account_deletion_unavailable", "code": "account_deletion_unavailable"})),
    )
        .into_response()
}

fn internal(stage: &'static str, cause: &(impl std::error::Error + 'static)) -> Response {
    tracing::error!(
        stage,
        error_chain = crate::foundation::safe_log_field::safe_display(
            &crate::foundation::error::describe(cause)
        ),
        "account deletion failed"
    );
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
}

async fn preflight<S: AccountDeletionStore, H, B, N, D>(
    State(deps): State<Deps<S, H, B, N, D>>,
    Extension(user): Extension<UserId>,
) -> Response {
    if !offered(deps.profile) {
        return Json(serde_json::json!({"available": false, "blockers": null})).into_response();
    }
    match deps.store.deletion_blockers(&user).await {
        Ok(blockers) => Json(serde_json::json!({
            "available": true,
            "blockers": blockers_json(&blockers),
        }))
        .into_response(),
        Err(cause) => internal("preflight", &cause),
    }
}

async fn delete_me<S, H, B, N, D>(
    State(deps): State<Deps<S, H, B, N, D>>,
    Extension(user): Extension<UserId>,
    headers: HeaderMap,
    ValidatedJson(input): ValidatedJson<Input>,
) -> Response
where
    S: AccountDeletionStore,
    H: PasswordHasher,
    B: AvatarBlobStore,
    N: UserChangedSink,
    D: DaemonConnectionControl,
{
    if !offered(deps.profile) {
        return embedded_refusal();
    }
    let Some(token) = strict_bearer_credential(&headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let rejected = || {
        (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "account_deletion_rejected", "code": "account_deletion_rejected"})),
        )
            .into_response()
    };
    let Ok(handle) = ParticipantHandle::parse(&input.handle) else {
        return rejected();
    };
    let session = SessionTokenHash::of_plaintext(token);
    let deleted = match delete_account(
        &deps.store,
        &deps.hasher,
        &user,
        &session,
        &handle,
        &input.password,
    )
    .await
    {
        Ok(AccountDeletionOutcome::Deleted(deleted)) => deleted,
        Ok(AccountDeletionOutcome::Rejected) => return rejected(),
        Ok(AccountDeletionOutcome::Blocked(blockers)) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "account_deletion_blocked",
                    "code": "account_deletion_blocked",
                    "blockers": blockers_json(&blockers),
                })),
            )
                .into_response();
        }
        Err(cause) => return internal("delete", &cause),
    };
    // Post-commit residue: best-effort, never a failure of the committed delete.
    if let Some(avatar) = &deleted.avatar
        && let Err(cause) = deps.avatars.delete(avatar).await
    {
        tracing::warn!(
            error_chain = crate::foundation::safe_log_field::safe_display(
                &crate::foundation::error::describe(&cause)
            ),
            "account deleted but avatar blob cleanup failed"
        );
    }
    for host in deleted.hosts {
        if let Err(cause) = deps.daemons.disconnect(host).await {
            tracing::warn!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &crate::foundation::error::describe(&cause)
                ),
                "account deleted but computer disconnect failed"
            );
        }
    }
    if let Err(cause) = deps.changed.user_changed(&deleted.user_id).await {
        tracing::warn!(
            error_chain = crate::foundation::safe_log_field::safe_display(
                &crate::foundation::error::describe(&cause)
            ),
            "account deleted but user_changed announce failed"
        );
    }
    StatusCode::NO_CONTENT.into_response()
}
