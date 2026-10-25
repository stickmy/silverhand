//! Human channel-creation wire boundary.
//!
//! JSON/title admission completes before the persistence operation receives a
//! bounded typed request. Mutable governance, membership and title uniqueness
//! remain the writer-reserved SQLite unit's authority.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Extension, FromRequest, Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use garde::Validate;
use serde::Deserialize;
use serde_json::json;
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        collaboration::channel_directory::{
            BoundHumanChannelCreation, ChannelVisibility, HumanChannelCreateAftermath,
            HumanChannelCreatePort, HumanChannelCreateRefusal, HumanChannelCreateRequest,
            create_human_channel,
        },
        conversation_title::CanonicalConversationTitle,
    },
    foundation::identity::{UserId, WorkspaceId},
};

use super::{ACTIVE_PATH, error, identity_channel, internal};
use crate::foundation::http_error::coded_error as coded;
use crate::foundation::wire_admission::ValidatedJson;
use crate::foundation::wire_patch::RequiredPatchField;

/// `POST` body — the reference `z.object({ title, member_agent_ids: array(string).default([]),
/// member_user_ids: array(string).optional(), visibility: enum.optional() })` (NOT `.strict()`,
/// unknown keys stripped). Serde owns the shape; the title's canonical grammar is a stateless
/// garde delegate to `CanonicalConversationTitle::parse` (→ minimal 422); workspace existence,
/// membership legality and title uniqueness stay the writer-reserved core authority.
#[derive(Debug, Deserialize, Validate)]
struct CreateChannelBody {
    #[garde(custom(valid_channel_title))]
    title: String,
    #[serde(default)]
    #[garde(skip)]
    member_agent_ids: Vec<String>,
    /// `z.array(z.string()).optional()` — OPTIONAL but NOT nullable: an absent key is legal
    /// (→ `None`), but an explicit `null` is refused (optional ≠ nullable). `RequiredPatchField`
    /// carries exactly that two-arm distinction, so `{"member_user_ids": null}` is a native
    /// decode fault rather than silently `None`.
    #[serde(default)]
    #[garde(skip)]
    member_user_ids: RequiredPatchField<Vec<String>>,
    /// `ConversationVisibilitySchema.optional()` — optional but NOT nullable, same algebra as
    /// `member_user_ids`: an explicit `null` is a native decode fault, not the server default.
    #[serde(default)]
    #[garde(skip)]
    visibility: RequiredPatchField<WireChannelVisibility>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireChannelVisibility {
    Private,
    Public,
}

impl From<WireChannelVisibility> for ChannelVisibility {
    fn from(value: WireChannelVisibility) -> Self {
        match value {
            WireChannelVisibility::Private => ChannelVisibility::Private,
            WireChannelVisibility::Public => ChannelVisibility::Public,
        }
    }
}

fn valid_channel_title(value: &str, _: &()) -> garde::Result {
    CanonicalConversationTitle::parse(value)
        .map(|_| ())
        .map_err(|_| garde::Error::new("title is not a valid channel title"))
}

impl CreateChannelBody {
    fn into_request(self) -> Result<HumanChannelCreateRequest, ()> {
        let title = CanonicalConversationTitle::parse(&self.title).map_err(|_| ())?;
        let member_user_ids = match self.member_user_ids {
            RequiredPatchField::Missing => None,
            RequiredPatchField::Value(users) => Some(users),
        };
        let visibility = match self.visibility {
            RequiredPatchField::Missing => None,
            RequiredPatchField::Value(visibility) => Some(ChannelVisibility::from(visibility)),
        };
        Ok(HumanChannelCreateRequest::new(
            title,
            self.member_agent_ids,
            member_user_ids,
            visibility,
        ))
    }
}

struct Inner<P, A> {
    port: P,
    aftermath: A,
}

struct CreateState<P, A>(Arc<Inner<P, A>>);

impl<P, A> Clone for CreateState<P, A> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

/// The POST leaf is kept physically separate from the three read operations.
/// Application composition may merge the routers at the owned mount seam.
pub fn create_router<P, A>(port: P, aftermath: A) -> Router
where
    P: HumanChannelCreatePort + Send + Sync + 'static,
    A: HumanChannelCreateAftermath + Send + Sync + 'static,
{
    Router::new()
        .route(ACTIVE_PATH, post(create::<P, A>))
        .with_state(CreateState(Arc::new(Inner { port, aftermath })))
}

async fn create<P, A>(
    State(state): State<CreateState<P, A>>,
    principal: Option<Extension<UserId>>,
    Path(raw_workspace): Path<String>,
    request: Request,
) -> Response
where
    P: HumanChannelCreatePort,
    A: HumanChannelCreateAftermath,
{
    let Some(Extension(viewer)) = principal else {
        tracing::error!("channel-directory create route reached without human principal");
        return internal();
    };
    let Ok(workspace) = WorkspaceId::parse(raw_workspace) else {
        return error(StatusCode::BAD_REQUEST, "workspace id required");
    };

    // Body-first: the shared `ValidatedJson` extractor decodes and runs the title grammar
    // before the persistence op receives a bounded typed request. Structural faults keep
    // native 415/400/422; a title-rule failure collapses to the minimal 422. Workspace
    // existence, governance, membership and title uniqueness remain the writer-reserved
    // SQLite unit's authority (a value-first admission ordering, matching TS).
    let body = match ValidatedJson::<CreateChannelBody>::from_request(request, &()).await {
        Ok(ValidatedJson(body)) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let request = match body.into_request() {
        Ok(request) => request,
        Err(()) => return crate::foundation::wire_admission::invalid_input(),
    };
    let requested_title = request.title().title().to_owned();

    match create_human_channel(
        &state.0.port,
        &state.0.aftermath,
        viewer,
        workspace,
        request,
    )
    .await
    {
        Ok(Ok(execution)) => {
            if let Some(failure) = execution.publication_failure() {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(failure))
                    ),
                    "committed channel creation publication failed"
                );
            }
            success(execution.committed())
        }
        Ok(Err(reason)) => refusal(reason, &requested_title),
        Err(failure) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&failure))
                ),
                "channel creation failed"
            );
            internal()
        }
    }
}

fn refusal(refusal: HumanChannelCreateRefusal, requested_title: &str) -> Response {
    match refusal {
        HumanChannelCreateRefusal::WorkspaceNotFound => {
            error(StatusCode::NOT_FOUND, "workspace not found")
        }
        HumanChannelCreateRefusal::Forbidden => error(StatusCode::FORBIDDEN, "forbidden"),
        HumanChannelCreateRefusal::AgentHomePublic => error(
            StatusCode::BAD_REQUEST,
            "agent_home DM channels cannot be public",
        ),
        HumanChannelCreateRefusal::AgentHomeExplicitMembers => error(
            StatusCode::BAD_REQUEST,
            "agent_home channels always use the home owner/current user as members",
        ),
        HumanChannelCreateRefusal::AgentHomeOwnerNotFound => {
            error(StatusCode::NOT_FOUND, "agent_home owner not found")
        }
        HumanChannelCreateRefusal::AgentNotFound(id) => coded(
            StatusCode::BAD_REQUEST,
            format!("agent {id} not found"),
            "agent_not_found",
        ),
        HumanChannelCreateRefusal::AgentNotWorkspaceMember(id) => coded(
            StatusCode::BAD_REQUEST,
            format!("agent {id} is not a member of this workspace"),
            "agent_not_workspace_member",
        ),
        HumanChannelCreateRefusal::UserNotWorkspaceMember(id) => coded(
            StatusCode::BAD_REQUEST,
            format!("user {id} is not a human member of this workspace"),
            "user_not_workspace_member",
        ),
        HumanChannelCreateRefusal::TitleTaken => coded(
            StatusCode::CONFLICT,
            format!("a channel titled {requested_title} already exists in this workspace"),
            "conversation_title_taken",
        ),
    }
}

fn success(value: &BoundHumanChannelCreation) -> Response {
    (
        StatusCode::CREATED,
        Json(json!({"channel": identity_channel(value.channel(), value.workspace())})),
    )
        .into_response()
}

#[cfg(test)]
mod tests;
