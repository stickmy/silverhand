//! `POST /internal/agents/{aid}/follow-thread` and `.../unfollow-thread` — an agent
//! joins/leaves a thread's delivery subscription.
//!
//! Behind the runtime-credential perimeter (agent bound to `{aid}` first). The
//! conversation is derived from the root message, never named by the caller. Gates,
//! in the reference order: root message exists (`404 reply_target_not_found`) →
//! `read_messages` capability on its workspace (`403 capability_denied`) → the
//! write. The write re-gates the agent's live participation under its own lock; a
//! non-participant collapses to the SAME `404 reply_target_not_found` (deliberate
//! anti-oracle — a caller learns nothing about a conversation it cannot see). A
//! direct-pair (workspace-less) root has no capability axis — participation is the
//! whole gate.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Extension, FromRequest, Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::{Deserialize, Serialize};
use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::thread_follow::{
        AgentThreadFollowChange, AgentThreadFollowRefusal, AgentThreadFollowWrite,
        RootMessageLocate,
    },
    host_runtime::runtime_credential::RuntimePrincipal,
};

use crate::foundation::http_error::logged_500 as internal;
use crate::foundation::wire_admission::ValidatedJson;
use crate::host_runtime::runtime_credential::bind_runtime_actor;

pub const AGENT_FOLLOW_THREAD_PATH: &str = "/internal/agents/{aid}/follow-thread";
pub const AGENT_UNFOLLOW_THREAD_PATH: &str = "/internal/agents/{aid}/unfollow-thread";

/// The route resolves the root's CONVERSATION (via `locate`) to call the writer; the
/// writer fresh-replays root existence + participation + `read_messages` in-tx,
/// so the route holds no capability authority.
pub struct AgentThreadFollowState<L, W> {
    pub locate: L,
    pub writer: W,
}

struct Shared<L, W>(Arc<AgentThreadFollowState<L, W>>);
impl<L, W> Clone for Shared<L, W> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn agent_thread_follow_router<L, W>(state: AgentThreadFollowState<L, W>) -> Router
where
    L: RootMessageLocate + Send + Sync + 'static,
    W: AgentThreadFollowWrite + Send + Sync + 'static,
{
    Router::new()
        .route(AGENT_FOLLOW_THREAD_PATH, post(follow::<L, W>))
        .route(AGENT_UNFOLLOW_THREAD_PATH, post(unfollow::<L, W>))
        .with_state(Shared(Arc::new(state)))
}

/// `AgentFollowThreadInputSchema`: `{ root_message_id: string (min 1) }`.
#[derive(Deserialize, garde::Validate)]
struct FollowThreadBody {
    #[garde(skip)]
    root_message_id: String,
}

enum Verb {
    Follow,
    Unfollow,
}

async fn follow<L, W>(
    State(state): State<Shared<L, W>>,
    Path(aid): Path<String>,
    principal: Option<Extension<RuntimePrincipal>>,
    request: Request,
) -> Response
where
    L: RootMessageLocate + Send + Sync + 'static,
    W: AgentThreadFollowWrite + Send + Sync + 'static,
{
    run(state, aid, principal, request, Verb::Follow).await
}

async fn unfollow<L, W>(
    State(state): State<Shared<L, W>>,
    Path(aid): Path<String>,
    principal: Option<Extension<RuntimePrincipal>>,
    request: Request,
) -> Response
where
    L: RootMessageLocate + Send + Sync + 'static,
    W: AgentThreadFollowWrite + Send + Sync + 'static,
{
    run(state, aid, principal, request, Verb::Unfollow).await
}

async fn run<L, W>(
    state: Shared<L, W>,
    aid: String,
    principal: Option<Extension<RuntimePrincipal>>,
    request: Request,
    verb: Verb,
) -> Response
where
    L: RootMessageLocate + Send + Sync + 'static,
    W: AgentThreadFollowWrite + Send + Sync + 'static,
{
    // IDENTITY FIRST (#25): bind `{aid}` before decoding the body.
    let principal = match bind_runtime_actor(principal.as_ref().map(|e| &e.0), &aid) {
        Ok(principal) => principal,
        Err(refusal) => return refusal.into_response(),
    };
    let agent = principal.agent().clone();
    let body = match ValidatedJson::<FollowThreadBody>::from_request(request, &()).await {
        Ok(ValidatedJson(body)) => body,
        Err(rejection) => return rejection.into_response(),
    };
    if body.root_message_id.is_empty() {
        return invalid_input();
    }

    let state = &state.0;

    // Root message existence → its conversation (the writer needs the conversation to
    // fresh-replay participation + capability). A missing message is the reference
    // `404 reply_target_not_found`; the writer re-verifies existence too.
    let location = match state
        .locate
        .locate_root_message(&body.root_message_id)
        .await
    {
        Ok(Some(location)) => location,
        Ok(None) => return reply_target_not_found(),
        Err(cause) => return internal(cause, "thread-follow root locate failed"),
    };

    let root_message =
        match silverhand_backend_core::conversation_messaging::messaging::ThreadRootId::parse(
            body.root_message_id.clone(),
        ) {
            Ok(root) => root,
            Err(_) => return invalid_input(),
        };
    let outcome = match verb {
        Verb::Follow => {
            state
                .writer
                .follow(agent, location.conversation, root_message)
                .await
        }
        Verb::Unfollow => {
            state
                .writer
                .unfollow(agent, location.conversation, root_message)
                .await
        }
    };
    match outcome {
        Ok(Ok(change)) => (StatusCode::OK, Json(change_body(&change))).into_response(),
        // A non-participant collapses to the SAME 404 the missing-root arm returns —
        // the deliberate anti-oracle (`canViewConversation` failure ≡
        // reply_target_not_found), and it is refused BEFORE capability (#8).
        Ok(Err(AgentThreadFollowRefusal::RootNotFound))
        | Ok(Err(AgentThreadFollowRefusal::NotParticipant)) => reply_target_not_found(),
        Ok(Err(AgentThreadFollowRefusal::CapabilityDenied)) => capability_denied("read_messages"),
        Ok(Err(AgentThreadFollowRefusal::InvalidClock)) => {
            tracing::error!(
                "thread-follow reserved-writer clock produced a domain-invalid instant"
            );
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
        }
        Err(cause) => internal(cause, "thread-follow write failed"),
    }
}

fn change_body(change: &AgentThreadFollowChange) -> ChangeBody {
    ChangeBody {
        thread_root_id: change.thread_root.as_str().to_owned(),
        changed: change.changed,
    }
}

/// `{ thread_root_id, changed }`.
#[derive(Serialize)]
struct ChangeBody {
    thread_root_id: String,
    changed: bool,
}

fn invalid_input() -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(serde_json::json!({ "error": "invalid input" })),
    )
        .into_response()
}

fn reply_target_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "error": "thread root message not found in conversation",
            "code": "reply_target_not_found"
        })),
    )
        .into_response()
}

fn capability_denied(capability: &'static str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({
            "code": "capability_denied",
            "capability": capability,
            "message": format!("missing '{capability}' capability in this workspace"),
            "details": { "capability": capability },
        })),
    )
        .into_response()
}
