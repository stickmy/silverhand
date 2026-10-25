//! `POST /internal/agents/{aid}/create-channel` — an agent forks a new workspace
//! channel from a source conversation.
//!
//! Behind the runtime-credential perimeter (agent bound to `{aid}` first). The
//! fork-source is resolved from the `target` address (a CONVERSATION target — a
//! `/thread:` suffix is refused); the workspace is the source's; the
//! `create_channels` capability gates it. The write validates every gate again
//! under its own lock, inserts the channel + members + inherited roster, and seeds
//! the context + prompt messages through the canonical seq/head primitives. On
//! success the `workspace_channels_changed` invalidation fans out. The wire returns
//! only `{ channel_id }`.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Extension, FromRequest, Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::{Deserialize, Serialize};
use silverhand_backend_core::conversation_messaging::messaging::environment::SendEntropy;
use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::channel_fork::{
        ChannelForkRefusal, ChannelForkRequest, ChannelForkWrite,
    },
    conversation_messaging::messaging::conversation_title::CanonicalConversationTitle,
    conversation_messaging::messaging::{
        AddressRefusal, DeliveryIntent, DirectPairAllocation, TargetAddress,
    },
    foundation::identity::WorkspaceId,
    host_runtime::runtime_credential::RuntimePrincipal,
    identity_access::agent_peer_profile::{CallerConversationPlacement, WorkspacePlacement},
};

use crate::foundation::http_error::{
    coded_error as coded, json_error as error, logged_500 as internal, logged_500 as internal_dyn,
};
use crate::foundation::wire_admission::ValidatedJson;
use crate::host_runtime::runtime_credential::bind_runtime_actor;

pub const AGENT_CREATE_CHANNEL_PATH: &str = "/internal/agents/{aid}/create-channel";

/// The awaited post-commit aftermath the fork owes: consuming the SEALED
/// post-images, it publishes `workspace_channels_changed`, then delivers the context
/// message to the actor, distributes the prompt to the other members, and starts all
/// member runtimes best-effort — in that strict order, each awaited, before the route
/// returns 200. A leg failure is observable and never rewrites the sealed 200. The
/// seed message PAYLOAD + coordinates come from the committed fork (no route-minted
/// ids); the prompt/start AUDIENCE is re-read from the LIVE conversation roster at
/// each effect point, NOT the committed fork roster — sealed seed
/// payload vs live audience are distinct.
pub trait ChannelForkAftermath: Send + Sync + 'static {
    fn channel_forked(
        &self,
        workspace: &WorkspaceId,
        committed: &silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_fork::CommittedChannelFork,
    ) -> impl std::future::Future<Output = ()> + Send;
}

pub struct AgentCreateChannelState<T, P, W, B, C, A> {
    pub resolver: T,
    pub placement: P,
    pub writer: W,
    pub entropy: B,
    pub clock: C,
    pub aftermath: A,
}

struct Shared<T, P, W, B, C, A>(Arc<AgentCreateChannelState<T, P, W, B, C, A>>);
impl<T, P, W, B, C, A> Clone for Shared<T, P, W, B, C, A> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn agent_create_channel_router<T, P, W, B, C, A>(
    state: AgentCreateChannelState<T, P, W, B, C, A>,
) -> Router
where
    T: TargetAddress + Send + Sync + 'static,
    P: CallerConversationPlacement + Send + Sync + 'static,
    W: ChannelForkWrite + Send + Sync + 'static,
    B: SendEntropy + Send + Sync + 'static,
    C: silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_fork::ChannelForkClock + 'static,
    A: ChannelForkAftermath,
{
    Router::new()
        .route(AGENT_CREATE_CHANNEL_PATH, post(create::<T, P, W, B, C, A>))
        .with_state(Shared(Arc::new(state)))
}

/// `AgentCreateChannelInputSchema`: `{ title, context_summary, initial_prompt, target }`.
#[derive(Deserialize, garde::Validate)]
struct CreateChannelBody {
    #[garde(skip)]
    title: String,
    #[garde(skip)]
    context_summary: String,
    #[garde(skip)]
    initial_prompt: String,
    #[garde(skip)]
    target: String,
}

async fn create<T, P, W, B, C, A>(
    State(state): State<Shared<T, P, W, B, C, A>>,
    Path(aid): Path<String>,
    principal: Option<Extension<RuntimePrincipal>>,
    request: Request,
) -> Response
where
    T: TargetAddress + Send + Sync + 'static,
    P: CallerConversationPlacement + Send + Sync + 'static,
    W: ChannelForkWrite + Send + Sync + 'static,
    B: SendEntropy + Send + Sync + 'static,
    C: silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_fork::ChannelForkClock + 'static,
    A: ChannelForkAftermath,
{
    // IDENTITY FIRST: bind the actor to `{aid}` BEFORE any body extraction, so a
    // mismatched credential refuses `agent_mismatch` 401 before the wire ever decodes
    // the body (a deferred `from_request` runs only past this gate). Axum would run a
    // `ValidatedJson` extractor param BEFORE the handler, inverting this order — hence
    // the raw `Request` + explicit post-bind extraction.
    let principal = match bind_runtime_actor(principal.as_ref().map(|e| &e.0), &aid) {
        Ok(principal) => principal,
        Err(refusal) => return refusal.into_response(),
    };
    let agent = principal.agent().clone();
    let body = match ValidatedJson::<CreateChannelBody>::from_request(request, &()).await {
        Ok(ValidatedJson(body)) => body,
        Err(rejection) => return rejection.into_response(),
    };
    // The title IS the channel's address (canonical, ≤ cap, addressable).
    let Ok(title) = CanonicalConversationTitle::parse(&body.title) else {
        return invalid_input();
    };
    if body.context_summary.is_empty() || body.initial_prompt.is_empty() || body.target.is_empty() {
        return invalid_input();
    }

    let state = &state.0;

    // Resolve the fork-source address (read intent; a direct pair is not allocated).
    let resolved = match state
        .resolver
        .resolve(
            &agent,
            &body.target,
            DirectPairAllocation::Forbidden,
            DeliveryIntent::Read,
        )
        .await
    {
        Ok(Ok(resolved)) => resolved,
        Ok(Err(refusal)) => return address_refusal(refusal),
        Err(cause) => return internal_dyn(cause, "create-channel target resolve failed"),
    };
    // A conversation target, not a reply cluster: a `/thread:` suffix is refused.
    if resolved.thread_root.is_some() {
        return coded(
            StatusCode::BAD_REQUEST,
            "this operation addresses a conversation, not a reply cluster",
            "thread_suffix_not_supported",
        );
    }
    let source = resolved.conversation;

    // The fork-source's workspace (a workspace-less pair cannot be a fork source).
    let workspace = match state.placement.conversation_placement(&source).await {
        Ok(Some(WorkspacePlacement::Workspace(id))) => id,
        Ok(Some(WorkspacePlacement::WorkspaceLess)) => {
            return workspace_scope_required(Some("direct_pair"));
        }
        Ok(None) => return error(StatusCode::NOT_FOUND, "conversation not found"),
        Err(cause) => return internal(cause, "create-channel placement read failed"),
    };

    // The writer owns the full authority IN its transaction: fresh
    // capability, per-source-agent membership gate, per-workspace title, id-minting
    // AFTER the gates. The route mints nothing and does not pre-check capability.
    let request = ChannelForkRequest {
        workspace: workspace.clone(),
        source_conversation: source,
        actor_agent: agent.clone(),
        title,
        context_summary: body.context_summary,
        initial_prompt: body.initial_prompt,
    };

    match state
        .writer
        .fork_channel(&request, &state.entropy, &state.clock)
        .await
    {
        Ok(Ok(committed)) => {
            // The awaited aftermath consumes the SEALED post-images: the
            // committed channel + both seed messages + member coordinates drive the
            // ordered deliver → distribute → start-all; a failure is observable and
            // does not rewrite the 200.
            state.aftermath.channel_forked(&workspace, &committed).await;
            (
                StatusCode::OK,
                Json(CreatedChannel {
                    channel_id: committed.channel.into_inner(),
                }),
            )
                .into_response()
        }
        Ok(Err(refusal)) => fork_refusal(refusal),
        Err(cause) => internal(cause, "create-channel fork write failed"),
    }
}

fn fork_refusal(refusal: ChannelForkRefusal) -> Response {
    match refusal {
        ChannelForkRefusal::WorkspaceNotFound => coded(
            StatusCode::NOT_FOUND,
            "workspace not found",
            "workspace_not_found",
        ),
        ChannelForkRefusal::WorkspaceNotRegular => coded(
            StatusCode::BAD_REQUEST,
            "workspace is not a regular workspace",
            "workspace_not_regular",
        ),
        ChannelForkRefusal::CapabilityDenied => capability_denied("create_channels"),
        ChannelForkRefusal::AgentNotWorkspaceMember { agent } => coded(
            StatusCode::BAD_REQUEST,
            format!("agent {} is not a member of this workspace", agent.as_str()),
            "agent_not_workspace_member",
        ),
        ChannelForkRefusal::SourceConversationNotFound => coded(
            StatusCode::NOT_FOUND,
            "source conversation not found",
            "source_conversation_not_found",
        ),
        ChannelForkRefusal::AgentNotMemberOfSource => coded(
            StatusCode::FORBIDDEN,
            "agent is not a member of the source conversation",
            "agent_not_member_of_source_conv",
        ),
        ChannelForkRefusal::AgentWorkspaceMismatch => coded(
            StatusCode::FORBIDDEN,
            "source conversation belongs to another workspace",
            "agent_workspace_mismatch",
        ),
        // Exact reference dynamic message: the writer-bound canonical title.
        ChannelForkRefusal::TitleTaken { title } => coded(
            StatusCode::CONFLICT,
            format!("a channel titled {title} already exists in this workspace"),
            "conversation_title_taken",
        ),
    }
}

/// `{ channel_id }`.
#[derive(Serialize)]
struct CreatedChannel {
    channel_id: String,
}

fn invalid_input() -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(serde_json::json!({ "error": "invalid input" })),
    )
        .into_response()
}

/// The workspace-less refusal: the exact `resolveAgentQueryWorkspaceId`
/// create-channel variant carrying the source placement kind.
fn workspace_scope_required(source_placement_kind: Option<&str>) -> Response {
    let error = match source_placement_kind {
        Some(kind) => format!(
            "this conversation has no workspace (placement_kind={kind}), and this act is workspace-scoped: name a workspace-placed conversation"
        ),
        None => "agent-scoped channel creation must name a workspace conversation".to_owned(),
    };
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": error, "code": "workspace_scope_required" })),
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

fn address_refusal(refusal: AddressRefusal) -> Response {
    let (status, message, code) = match refusal {
        AddressRefusal::Malformed => (
            StatusCode::BAD_REQUEST,
            "target is not an address",
            "target_malformed",
        ),
        AddressRefusal::SelfPair => (
            StatusCode::BAD_REQUEST,
            "a direct message needs another participant",
            "self_pair_not_addressable",
        ),
        AddressRefusal::DirectMessageScope => (
            StatusCode::FORBIDDEN,
            "Conversation-scoped agents do not support DMs. Use a shared channel.",
            "direct_message_scope_forbidden",
        ),
        AddressRefusal::Unresolved => (
            StatusCode::NOT_FOUND,
            "target names no stream you can use",
            "target_unresolved",
        ),
        AddressRefusal::ThreadNotInTarget => (
            StatusCode::NOT_FOUND,
            "target names no reply cluster of that conversation",
            "thread_not_in_target",
        ),
    };
    coded(status, message, code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request as HttpRequest;
    use silverhand_backend_core::conversation_messaging::messaging::ResolvedStream;
    use silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_fork::{
        ChannelForkRequest, ChannelForkWrite, CommittedChannelFork, SealedSeedMessage,
    };
    use silverhand_backend_core::conversation_messaging::messaging::environment::{
        EnvironmentUnavailable, SendEntropy,
    };
    use silverhand_backend_core::conversation_messaging::messaging::{
        DeliveryIntent, DirectPairAllocation, MessageId,
    };
    use silverhand_backend_core::foundation::identity::{
        AgentId, ConversationId, DaemonInstanceId,
    };
    use silverhand_backend_core::foundation::persistence::PersistenceFailure;
    use silverhand_backend_core::host_runtime::runtime_credential::{
        RuntimeCustody, RuntimeGeneration, RuntimeScope,
    };
    use silverhand_backend_core::identity_access::agent_peer_profile::WorkspacePlacement;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    const AGENT: &str = "agent-1";

    fn principal() -> RuntimePrincipal {
        RuntimePrincipal::for_scope(
            RuntimeScope::ConversationSlot {
                agent: AgentId::parse(AGENT).unwrap(),
                source: ConversationId::parse("src").unwrap(),
            },
            RuntimeCustody::new(
                RuntimeGeneration::new(1).unwrap(),
                DaemonInstanceId::parse("daemon-1").unwrap(),
            ),
        )
    }

    struct Resolver {
        thread: Option<String>,
    }
    impl TargetAddress for Resolver {
        type Error = std::convert::Infallible;
        async fn resolve(
            &self,
            _a: &AgentId,
            _t: &str,
            _alloc: DirectPairAllocation,
            _intent: DeliveryIntent,
        ) -> Result<Result<ResolvedStream, AddressRefusal>, Self::Error> {
            Ok(Ok(ResolvedStream {
                conversation: ConversationId::parse("src").unwrap(),
                thread_root: self.thread.as_ref().map(|t| {
                    silverhand_backend_core::conversation_messaging::messaging::ThreadRootId::parse(
                        t.clone(),
                    )
                    .unwrap()
                }),
            }))
        }
    }

    struct Placement(Option<WorkspacePlacement>);
    impl CallerConversationPlacement for Placement {
        async fn conversation_placement(
            &self,
            _c: &ConversationId,
        ) -> Result<Option<WorkspacePlacement>, PersistenceFailure> {
            Ok(self.0.clone())
        }
    }

    // A writer that echoes a committed fork (or a refusal), recording that it ran.
    #[derive(Clone)]
    struct Writer {
        refuse: Option<silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_fork::ChannelForkRefusal>,
        ran: std::sync::Arc<AtomicUsize>,
    }
    impl ChannelForkWrite for Writer {
        async fn fork_channel<B: SendEntropy + Send + Sync, C: silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_fork::ChannelForkClock>(
            &self,
            request: &ChannelForkRequest,
            _entropy: &B,
            _clock: &C,
        ) -> Result<Result<CommittedChannelFork, silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_fork::ChannelForkRefusal>, PersistenceFailure>{
            self.ran.fetch_add(1, Ordering::SeqCst);
            if let Some(refusal) = &self.refuse {
                return Ok(Err(refusal.clone()));
            }
            let channel = ConversationId::parse("chan-new").unwrap();
            Ok(Ok(CommittedChannelFork {
                channel: channel.clone(),
                actor_agent: request.actor_agent.clone(),
                context_message: SealedSeedMessage {
                    id: MessageId::parse("ctx").unwrap(),
                    conversation: channel.clone(),
                    seq: 1,
                    content: format!(
                        "Channel created from 'Src'. Reason: {}",
                        request.context_summary
                    ),
                },
                prompt_message: SealedSeedMessage {
                    id: MessageId::parse("prompt").unwrap(),
                    conversation: channel,
                    seq: 2,
                    content: request.initial_prompt.clone(),
                },
                member_agents: vec![request.actor_agent.clone()],
                member_users: vec![],
            }))
        }
    }

    struct Entropy;
    impl SendEntropy for Entropy {
        fn fill(&self, buffer: &mut [u8]) -> Result<(), EnvironmentUnavailable> {
            for (i, b) in buffer.iter_mut().enumerate() {
                *b = (i as u8).wrapping_add(1);
            }
            Ok(())
        }
    }

    struct Clock;
    impl silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_fork::ChannelForkClock
        for Clock
    {
        fn now_ms(&self) -> i64 {
            1000
        }
    }

    #[derive(Clone)]
    struct Aftermath(std::sync::Arc<AtomicUsize>);
    impl ChannelForkAftermath for Aftermath {
        async fn channel_forked(&self, _w: &WorkspaceId, _c: &CommittedChannelFork) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[allow(clippy::type_complexity)]
    fn state(
        thread: Option<&str>,
        placement: Option<WorkspacePlacement>,
        refuse: Option<silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_fork::ChannelForkRefusal>,
        ran: std::sync::Arc<AtomicUsize>,
        effects: std::sync::Arc<AtomicUsize>,
    ) -> AgentCreateChannelState<Resolver, Placement, Writer, Entropy, Clock, Aftermath> {
        AgentCreateChannelState {
            resolver: Resolver {
                thread: thread.map(str::to_owned),
            },
            placement: Placement(placement),
            writer: Writer { refuse, ran },
            entropy: Entropy,
            clock: Clock,
            aftermath: Aftermath(effects),
        }
    }

    async fn call(
        aid: &str,
        st: AgentCreateChannelState<Resolver, Placement, Writer, Entropy, Clock, Aftermath>,
        body: &str,
    ) -> (StatusCode, serde_json::Value) {
        let router = agent_create_channel_router(st).layer(Extension(principal()));
        let response = router
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri(format!("/internal/agents/{aid}/create-channel"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_owned()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
        };
        (status, json)
    }

    const GOOD_BODY: &str = r##"{"title":"Design Review","context_summary":"c","initial_prompt":"p","target":"#conv:src"}"##;

    #[tokio::test]
    async fn a_capable_agent_forks_wire_channel_id_and_awaited_aftermath() {
        let ran = std::sync::Arc::new(AtomicUsize::new(0));
        let effects = std::sync::Arc::new(AtomicUsize::new(0));
        let (status, body) = call(
            AGENT,
            state(
                None,
                Some(WorkspacePlacement::Workspace(
                    WorkspaceId::parse("ws-1").unwrap(),
                )),
                None,
                ran.clone(),
                effects.clone(),
            ),
            GOOD_BODY,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["channel_id"], "chan-new");
        assert_eq!(
            effects.load(Ordering::SeqCst),
            1,
            "awaited aftermath ran once"
        );
    }

    #[tokio::test]
    async fn thread_targeted_is_refused_writer_zero_reach() {
        let ran = std::sync::Arc::new(AtomicUsize::new(0));
        let effects = std::sync::Arc::new(AtomicUsize::new(0));
        let (status, body) = call(
            AGENT,
            state(
                Some("root-1"),
                Some(WorkspacePlacement::Workspace(
                    WorkspaceId::parse("ws-1").unwrap(),
                )),
                None,
                ran.clone(),
                effects.clone(),
            ),
            GOOD_BODY,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "thread_suffix_not_supported");
        assert_eq!(
            ran.load(Ordering::SeqCst),
            0,
            "writer zero-reach on refusal"
        );
    }

    #[tokio::test]
    async fn workspace_less_source_carries_placement_kind() {
        let ran = std::sync::Arc::new(AtomicUsize::new(0));
        let effects = std::sync::Arc::new(AtomicUsize::new(0));
        let (status, body) = call(
            AGENT,
            state(
                None,
                Some(WorkspacePlacement::WorkspaceLess),
                None,
                ran,
                effects,
            ),
            GOOD_BODY,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "workspace_scope_required");
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("placement_kind=direct_pair")
        );
    }

    #[tokio::test]
    async fn writer_capability_denied_maps_403() {
        use silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_fork::ChannelForkRefusal;
        let ran = std::sync::Arc::new(AtomicUsize::new(0));
        let effects = std::sync::Arc::new(AtomicUsize::new(0));
        let (status, body) = call(
            AGENT,
            state(
                None,
                Some(WorkspacePlacement::Workspace(
                    WorkspaceId::parse("ws-1").unwrap(),
                )),
                Some(ChannelForkRefusal::CapabilityDenied),
                ran,
                effects.clone(),
            ),
            GOOD_BODY,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], "capability_denied");
        assert_eq!(effects.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn title_taken_carries_exact_dynamic_message() {
        use silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_fork::ChannelForkRefusal;
        let ran = std::sync::Arc::new(AtomicUsize::new(0));
        let effects = std::sync::Arc::new(AtomicUsize::new(0));
        let (status, body) = call(
            AGENT,
            state(
                None,
                Some(WorkspacePlacement::Workspace(
                    WorkspaceId::parse("ws-1").unwrap(),
                )),
                Some(ChannelForkRefusal::TitleTaken {
                    title: "Design Review".to_owned(),
                }),
                ran,
                effects,
            ),
            GOOD_BODY,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "conversation_title_taken");
        assert_eq!(
            body["error"],
            "a channel titled Design Review already exists in this workspace"
        );
    }

    #[tokio::test]
    async fn identity_binds_before_body_extraction() {
        let ran = std::sync::Arc::new(AtomicUsize::new(0));
        let effects = std::sync::Arc::new(AtomicUsize::new(0));
        let (status, _b) = call(
            "agent-OTHER",
            state(
                None,
                Some(WorkspacePlacement::Workspace(
                    WorkspaceId::parse("ws-1").unwrap(),
                )),
                None,
                ran.clone(),
                effects.clone(),
            ),
            "not json",
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            ran.load(Ordering::SeqCst),
            0,
            "writer zero-reach on identity refusal"
        );
    }
}
