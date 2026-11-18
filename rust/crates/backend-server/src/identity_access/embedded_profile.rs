//! Initial profile completion for an automatically authenticated embedded user.
//! The ordinary user record, session authority and post-commit invalidation are
//! shared with the existing profile routes; no client completion cache exists.
use crate::{
    foundation::deployment_profile::DeploymentProfile,
    identity_access::{
        auth::{PublicJsonUnauthorized, require_human_session},
        human_auth::{UserDto, log_lost_announce},
    },
};
use axum::{
    Json, Router,
    extract::{Extension, State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use silverhand_backend_core::{
    foundation::identity::UserId,
    identity_access::{
        auth::{AuthenticateHumanSession, SessionClock},
        user_identity::{
            CompleteEmbeddedProfileSetup, DisplayName, EmbeddedProfileSetupStore,
            ParticipantHandle, UserChangedAnnounce, UserChangedSink, UserRecord,
        },
    },
};

#[derive(Clone)]
struct Deps<S, N, C> {
    store: S,
    changed: N,
    clock: C,
    profile: DeploymentProfile,
}

pub fn router<S, N, C, A>(
    store: S,
    changed: N,
    clock: C,
    profile: DeploymentProfile,
    sessions: A,
) -> Router
where
    S: EmbeddedProfileSetupStore,
    N: UserChangedSink,
    C: SessionClock,
    A: AuthenticateHumanSession,
{
    let routes = Router::new()
        .route(
            "/api/me/embedded-profile-setup",
            get(read::<S, N, C>).put(complete::<S, N, C>),
        )
        .with_state(Deps {
            store,
            changed,
            clock,
            profile,
        });
    require_human_session(routes, sessions, PublicJsonUnauthorized)
}

fn error(status: StatusCode, code: &'static str) -> Response {
    (
        status,
        Json(serde_json::json!({"error": code, "code": code})),
    )
        .into_response()
}

fn success(required: bool, user: &UserRecord) -> Response {
    Json(serde_json::json!({"required": required, "user": UserDto::from_record(user)}))
        .into_response()
}

async fn read<S: EmbeddedProfileSetupStore, N: UserChangedSink, C: SessionClock>(
    State(deps): State<Deps<S, N, C>>,
    Extension(viewer): Extension<UserId>,
) -> Response {
    if deps.profile != DeploymentProfile::Embedded {
        return error(StatusCode::FORBIDDEN, "embedded_only");
    }
    match deps.store.read_setup(&viewer).await {
        Ok(Some(setup)) => success(setup.required, &setup.user),
        Ok(None) => error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
        Err(cause) => {
            tracing::error!(cause = %crate::foundation::safe_log_field::safe_display(&crate::foundation::error::describe(&cause)), "embedded profile read failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
        }
    }
}

#[derive(Deserialize)]
struct Input {
    name: String,
    display_name: String,
}

async fn complete<S: EmbeddedProfileSetupStore, N: UserChangedSink, C: SessionClock>(
    State(deps): State<Deps<S, N, C>>,
    Extension(viewer): Extension<UserId>,
    body: Result<Json<Input>, JsonRejection>,
) -> Response {
    if deps.profile != DeploymentProfile::Embedded {
        return error(StatusCode::FORBIDDEN, "embedded_only");
    }
    let Ok(Json(input)) = body else {
        return error(StatusCode::BAD_REQUEST, "invalid_input");
    };
    let (Ok(handle), Ok(display)) = (
        ParticipantHandle::parse(&input.name),
        DisplayName::parse(&input.display_name),
    ) else {
        return error(StatusCode::BAD_REQUEST, "invalid_input");
    };
    match deps
        .store
        .complete_setup(&viewer, &handle, &display, deps.clock.now_unix_millis())
        .await
    {
        Ok(CompleteEmbeddedProfileSetup::Completed(user)) => {
            // Completion is durable even when the response or invalidation is lost.
            // The response contains the new viewer; reconnect/retry reads the bit.
            if let Err(failure) = deps.changed.user_changed(&viewer).await {
                log_lost_announce(
                    &UserChangedAnnounce::Failed(failure),
                    "embedded-profile-setup",
                );
            }
            success(false, &user)
        }
        Ok(CompleteEmbeddedProfileSetup::AlreadyCompleted) => {
            error(StatusCode::CONFLICT, "profile_setup_completed")
        }
        Ok(CompleteEmbeddedProfileSetup::HandleTaken) => error(StatusCode::CONFLICT, "name_taken"),
        Ok(CompleteEmbeddedProfileSetup::NoSuchUser) => {
            error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
        }
        Err(cause) => {
            tracing::error!(cause = %crate::foundation::safe_log_field::safe_display(&crate::foundation::error::describe(&cause)), "embedded profile completion failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
        }
    }
}
