//! HTTP components for conversation roster membership.  These builders stay
//! unmounted and expose every production dependency explicitly.

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use garde::Validate;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        DirectPairAllocation, TargetAddress,
        collaboration::membership::{
            ChannelMembershipOutcome, ChannelMembershipRefusal, ConversationAgentRemovalPreflight,
            ConversationMember, ConversationMemberCandidate, ConversationMemberMutation,
            ConversationMembershipAftermath, ConversationMembershipChanged,
            ConversationMembershipPort, ConversationRoster, ConversationRosterRead,
            ManageConversationMembersCapability, MemberRemovability, MembershipRuntimeStop,
            NormalizedMemberHandle,
        },
    },
    foundation::identity::{AgentId, ConversationId, Principal, UserId},
    host_runtime::runtime_credential::RuntimePrincipal,
    identity_access::access::EnrollmentClock,
};

use crate::foundation::http_error::plain_500 as internal;
use crate::foundation::{error, wire_admission::ValidatedJson};

pub const CONVERSATION_MEMBERS_PATH: &str = "/api/conversations/{id}/members";
pub const CHANNEL_JOIN_PATH: &str = "/api/channels/{id}/join";
pub const CHANNEL_LEAVE_PATH: &str = "/api/channels/{id}/leave";
pub const CONVERSATION_AGENT_PATH: &str = "/api/conversations/{id}/members/agents/{agent_id}";
pub const CONVERSATION_HUMAN_PATH: &str = "/api/conversations/{id}/members/users/{user_id}";
pub const INTERNAL_MEMBERS_PATH: &str = "/internal/agents/{aid}/members";

#[derive(Clone)]
struct PublicState<P, C, A> {
    port: P,
    clock: C,
    aftermath: A,
}

pub fn public_router<P, C, A>(port: P, clock: C, aftermath: A) -> Router
where
    P: ConversationMembershipPort,
    C: EnrollmentClock,
    A: ConversationMembershipAftermath + MembershipRuntimeStop,
{
    Router::new()
        .route(
            CONVERSATION_MEMBERS_PATH,
            get(list::<P, C, A>).post(add::<P, C, A>),
        )
        .route(CHANNEL_JOIN_PATH, post(join::<P, C, A>))
        .route(CHANNEL_LEAVE_PATH, post(leave::<P, C, A>))
        .route(CONVERSATION_AGENT_PATH, delete(remove_agent::<P, C, A>))
        .route(CONVERSATION_HUMAN_PATH, delete(remove_human::<P, C, A>))
        .with_state(PublicState {
            port,
            clock,
            aftermath,
        })
}

#[allow(clippy::result_large_err)]
fn viewer(viewer: Option<Extension<UserId>>, operation: &'static str) -> Result<UserId, Response> {
    let Some(Extension(viewer)) = viewer else {
        tracing::error!(
            operation,
            "conversation membership route reached without human principal"
        );
        return Err(internal());
    };
    Ok(viewer)
}

#[allow(clippy::result_large_err)]
fn conversation(raw: String) -> Result<ConversationId, Response> {
    ConversationId::parse(raw)
        .map_err(|_| response(StatusCode::NOT_FOUND, "conversation not found", None))
}

async fn list<P, C, A>(
    State(state): State<PublicState<P, C, A>>,
    principal: Option<Extension<UserId>>,
    Path(raw): Path<String>,
) -> Response
where
    P: ConversationMembershipPort,
    C: EnrollmentClock,
    A: ConversationMembershipAftermath + MembershipRuntimeStop,
{
    let viewer = match viewer(principal, "conversation_members_list") {
        Ok(value) => value,
        Err(value) => return value,
    };
    let conversation = match conversation(raw) {
        Ok(value) => value,
        Err(value) => return value,
    };
    match state
        .port
        .list_conversation_members(&Principal::Human { user_id: viewer }, &conversation)
        .await
    {
        Ok(ConversationRosterRead::ConversationNotFound) => {
            response(StatusCode::NOT_FOUND, "conversation not found", None)
        }
        Ok(ConversationRosterRead::Forbidden) => response(StatusCode::FORBIDDEN, "forbidden", None),
        Ok(ConversationRosterRead::Visible {
            roster,
            removability,
        }) => Json(RosterReadDto::new(roster, removability)).into_response(),
        Err(cause) => failed("conversation roster read failed", &cause),
    }
}

#[derive(Debug, Deserialize, Validate)]
struct AddBody {
    #[serde(default)]
    #[garde(skip)]
    kind: Option<Value>,
    #[serde(default)]
    #[garde(skip)]
    agent_id: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    user_id: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    handle: Option<String>,
}

impl AddBody {
    fn candidate(self) -> Option<ConversationMemberCandidate> {
        match self.kind.as_ref().and_then(Value::as_str) {
            Some("agent") => self
                .agent_id
                .and_then(|raw| AgentId::parse(raw).ok())
                .map(ConversationMemberCandidate::Agent),
            Some("human") => self
                .user_id
                .and_then(|raw| UserId::parse(raw).ok())
                .map(ConversationMemberCandidate::Human),
            Some(_) => None,
            None => self
                .handle
                .and_then(|value| NormalizedMemberHandle::try_from(value).ok())
                .map(ConversationMemberCandidate::Handle),
        }
    }
}

async fn add<P, C, A>(
    State(state): State<PublicState<P, C, A>>,
    principal: Option<Extension<UserId>>,
    Path(raw): Path<String>,
    ValidatedJson(body): ValidatedJson<AddBody>,
) -> Response
where
    P: ConversationMembershipPort,
    C: EnrollmentClock,
    A: ConversationMembershipAftermath + MembershipRuntimeStop,
{
    let viewer = match viewer(principal, "conversation_member_add") {
        Ok(value) => value,
        Err(value) => return value,
    };
    let conversation = match conversation(raw) {
        Ok(value) => value,
        Err(value) => return value,
    };
    let Some(candidate) = body.candidate() else {
        return invalid();
    };
    match state
        .port
        .add_conversation_member(
            &Principal::Human { user_id: viewer },
            &conversation,
            candidate,
            state.clock.now_unix_millis(),
        )
        .await
    {
        Ok(outcome) => mutation(outcome, MutationOperation::Add, &state.aftermath).await,
        Err(cause) => failed("conversation member add failed", &cause),
    }
}

async fn join<P, C, A>(
    State(state): State<PublicState<P, C, A>>,
    principal: Option<Extension<UserId>>,
    Path(raw): Path<String>,
) -> Response
where
    P: ConversationMembershipPort,
    C: EnrollmentClock,
    A: ConversationMembershipAftermath + MembershipRuntimeStop,
{
    let viewer = match viewer(principal, "channel_join") {
        Ok(value) => value,
        Err(value) => return value,
    };
    let conversation = match conversation(raw) {
        Ok(value) => value,
        Err(value) => return value,
    };
    match state
        .port
        .join_public_channel(&viewer, &conversation, state.clock.now_unix_millis())
        .await
    {
        Ok(outcome) => channel(outcome, &viewer, &conversation, &state.aftermath).await,
        Err(cause) => failed("channel join failed", &cause),
    }
}

async fn leave<P, C, A>(
    State(state): State<PublicState<P, C, A>>,
    principal: Option<Extension<UserId>>,
    Path(raw): Path<String>,
) -> Response
where
    P: ConversationMembershipPort,
    C: EnrollmentClock,
    A: ConversationMembershipAftermath + MembershipRuntimeStop,
{
    let viewer = match viewer(principal, "channel_leave") {
        Ok(value) => value,
        Err(value) => return value,
    };
    let conversation = match conversation(raw) {
        Ok(value) => value,
        Err(value) => return value,
    };
    match state.port.leave_channel(&viewer, &conversation).await {
        Ok(outcome) => channel(outcome, &viewer, &conversation, &state.aftermath).await,
        Err(cause) => failed("channel leave failed", &cause),
    }
}

async fn remove_agent<P, C, A>(
    State(state): State<PublicState<P, C, A>>,
    principal: Option<Extension<UserId>>,
    Path((raw_conversation, raw_agent)): Path<(String, String)>,
) -> Response
where
    P: ConversationMembershipPort,
    C: EnrollmentClock,
    A: ConversationMembershipAftermath + MembershipRuntimeStop,
{
    let viewer = match viewer(principal, "conversation_agent_remove") {
        Ok(value) => value,
        Err(value) => return value,
    };
    let conversation = match conversation(raw_conversation) {
        Ok(value) => value,
        Err(value) => return value,
    };
    let Ok(agent) = AgentId::parse(raw_agent) else {
        return response(
            StatusCode::NOT_FOUND,
            "agent not found",
            Some("candidate_not_found"),
        );
    };
    let stop_runtime = match state
        .port
        .preflight_remove_conversation_agent(&viewer, &conversation, &agent)
        .await
    {
        Ok(ConversationAgentRemovalPreflight::Ready { stop_runtime }) => stop_runtime,
        Ok(ConversationAgentRemovalPreflight::ConversationNotFound) => {
            return mutation(
                ConversationMemberMutation::ConversationNotFound,
                MutationOperation::RemoveAgent,
                &state.aftermath,
            )
            .await;
        }
        Ok(ConversationAgentRemovalPreflight::Unsupported) => {
            return mutation(
                ConversationMemberMutation::Unsupported,
                MutationOperation::RemoveAgent,
                &state.aftermath,
            )
            .await;
        }
        Ok(ConversationAgentRemovalPreflight::Closed) => {
            return mutation(
                ConversationMemberMutation::Closed,
                MutationOperation::RemoveAgent,
                &state.aftermath,
            )
            .await;
        }
        Ok(ConversationAgentRemovalPreflight::Forbidden) => {
            return mutation(
                ConversationMemberMutation::Forbidden,
                MutationOperation::RemoveAgent,
                &state.aftermath,
            )
            .await;
        }
        Ok(ConversationAgentRemovalPreflight::CandidateNotFound) => {
            return mutation(
                ConversationMemberMutation::CandidateNotFound,
                MutationOperation::RemoveAgent,
                &state.aftermath,
            )
            .await;
        }
        Err(cause) => return failed("conversation agent removal preflight failed", &cause),
    };
    if stop_runtime && let Err(cause) = state.aftermath.stop(&conversation, &agent).await {
        tracing::warn!(
            error_chain =
                crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
            conversation_id = conversation.as_str(),
            agent_id = agent.as_str(),
            "conversation member removal runtime stop failed; continuing"
        );
    }
    match state
        .port
        .remove_conversation_agent(&viewer, &conversation, &agent)
        .await
    {
        Ok(outcome) => mutation(outcome, MutationOperation::RemoveAgent, &state.aftermath).await,
        Err(cause) => failed("conversation agent removal failed", &cause),
    }
}

async fn remove_human<P, C, A>(
    State(state): State<PublicState<P, C, A>>,
    principal: Option<Extension<UserId>>,
    Path((raw_conversation, raw_user)): Path<(String, String)>,
) -> Response
where
    P: ConversationMembershipPort,
    C: EnrollmentClock,
    A: ConversationMembershipAftermath,
{
    let viewer = match viewer(principal, "conversation_human_remove") {
        Ok(value) => value,
        Err(value) => return value,
    };
    let conversation = match conversation(raw_conversation) {
        Ok(value) => value,
        Err(value) => return value,
    };
    let Ok(user) = UserId::parse(raw_user) else {
        return response(
            StatusCode::NOT_FOUND,
            "user not found",
            Some("candidate_not_found"),
        );
    };
    match state
        .port
        .remove_conversation_human(&viewer, &conversation, &user)
        .await
    {
        Ok(outcome) => mutation(outcome, MutationOperation::RemoveHuman, &state.aftermath).await,
        Err(cause) => failed("conversation human removal failed", &cause),
    }
}

#[derive(Clone)]
struct InternalState<P, T, C, A> {
    port: P,
    target: T,
    clock: C,
    aftermath: A,
}

pub fn internal_router<P, T, C, A>(port: P, target: T, clock: C, aftermath: A) -> Router
where
    P: ConversationMembershipPort + ManageConversationMembersCapability,
    T: TargetAddress + Clone + Send + Sync + 'static,
    C: EnrollmentClock,
    A: ConversationMembershipAftermath,
{
    Router::new()
        .route(INTERNAL_MEMBERS_PATH, post(internal_add::<P, T, C, A>))
        .with_state(InternalState {
            port,
            target,
            clock,
            aftermath,
        })
}

#[derive(Debug, Deserialize, Validate)]
struct InternalBody {
    #[garde(skip)]
    handle: String,
    #[garde(length(min = 1))]
    target: String,
}

async fn internal_add<P, T, C, A>(
    State(state): State<InternalState<P, T, C, A>>,
    credential: Option<Extension<RuntimePrincipal>>,
    ValidatedJson(body): ValidatedJson<InternalBody>,
) -> Response
where
    P: ConversationMembershipPort + ManageConversationMembersCapability,
    T: TargetAddress,
    C: EnrollmentClock,
    A: ConversationMembershipAftermath,
{
    let Some(Extension(credential)) = credential else {
        tracing::error!("internal member add reached without runtime principal");
        return internal();
    };
    let Ok(handle) = NormalizedMemberHandle::try_from(body.handle) else {
        return invalid();
    };
    let resolved = match state
        .target
        .resolve(
            credential.agent(),
            &body.target,
            DirectPairAllocation::Forbidden,
            silverhand_backend_core::conversation_messaging::messaging::DeliveryIntent::Read,
        )
        .await
    {
        Ok(Ok(value)) => value,
        Ok(Err(refusal)) => return super::super::router::address_refusal(refusal),
        Err(cause) => return failed("member-add target resolution failed", &cause),
    };
    if resolved.thread_root.is_some() {
        return response(
            StatusCode::BAD_REQUEST,
            "this operation addresses a conversation, not a reply cluster",
            Some("thread_suffix_not_supported"),
        );
    }
    match state
        .port
        .may_manage_conversation_members(credential.agent(), &resolved.conversation)
        .await
    {
        Ok(Ok(())) => {}
        Ok(Err(_)) => return capability_denied(),
        Err(cause) => return failed("member-add capability check failed", &cause),
    }
    match state
        .port
        .add_conversation_member(
            &Principal::Agent {
                agent_id: credential.agent().clone(),
            },
            &resolved.conversation,
            ConversationMemberCandidate::Handle(handle),
            state.clock.now_unix_millis(),
        )
        .await
    {
        Ok(outcome) => mutation(outcome, MutationOperation::Add, &state.aftermath).await,
        Err(cause) => failed("internal conversation member add failed", &cause),
    }
}

#[derive(Clone, Copy)]
enum MutationOperation {
    Add,
    RemoveAgent,
    RemoveHuman,
}

async fn mutation<A: ConversationMembershipAftermath>(
    outcome: ConversationMemberMutation,
    operation: MutationOperation,
    aftermath: &A,
) -> Response {
    match outcome {
        ConversationMemberMutation::Changed { roster, committed } => {
            if let Some(changed) = committed.as_ref() {
                announce(aftermath, changed).await;
            }
            Json(RosterDto::from(roster)).into_response()
        }
        ConversationMemberMutation::ConversationNotFound => {
            response(StatusCode::NOT_FOUND, "conversation not found", None)
        }
        ConversationMemberMutation::Unsupported => response(
            StatusCode::UNPROCESSABLE_ENTITY,
            match operation {
                MutationOperation::Add => {
                    "conversation members can only be added to channels or threads"
                }
                MutationOperation::RemoveAgent => {
                    "conversation members can only be removed from channels or threads"
                }
                MutationOperation::RemoveHuman => {
                    "human members can only be removed from channels"
                }
            },
            Some(match operation {
                MutationOperation::Add => "conversation_member_add_unsupported",
                MutationOperation::RemoveAgent | MutationOperation::RemoveHuman => {
                    "conversation_member_remove_unsupported"
                }
            }),
        ),
        ConversationMemberMutation::Closed => response(
            StatusCode::CONFLICT,
            "conversation is closed",
            Some("conversation_closed"),
        ),
        ConversationMemberMutation::Forbidden => response(StatusCode::FORBIDDEN, "forbidden", None),
        ConversationMemberMutation::CandidateNotFound => match operation {
            MutationOperation::Add => response(
                StatusCode::NOT_FOUND,
                "candidate not found",
                Some("candidate_not_found"),
            ),
            MutationOperation::RemoveAgent => response(
                StatusCode::NOT_FOUND,
                "agent not found",
                Some("candidate_not_found"),
            ),
            MutationOperation::RemoveHuman => {
                tracing::error!("human removal produced unreachable candidate-not-found outcome");
                internal()
            }
        },
        ConversationMemberMutation::HandleAmbiguous { candidates } => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "handle is ambiguous",
                "code": "handle_ambiguous",
                "candidates": candidates.into_iter().map(|candidate| match candidate {
                    silverhand_backend_core::conversation_messaging::messaging::collaboration::membership::AmbiguousMemberCandidate::Agent { id, display_name, handle } => serde_json::json!({
                        "kind": "agent", "id": id.as_str(), "display_name": display_name, "handle": handle,
                    }),
                    silverhand_backend_core::conversation_messaging::messaging::collaboration::membership::AmbiguousMemberCandidate::Human { id, display_name, handle } => serde_json::json!({
                        "kind": "human", "id": id.as_str(), "display_name": display_name, "handle": handle,
                    }),
                }).collect::<Vec<_>>()
            })),
        )
            .into_response(),
        ConversationMemberMutation::CandidateNotWorkspaceMember => response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "candidate must be a workspace member",
            Some("workspace_membership_required"),
        ),
        ConversationMemberMutation::CandidateNotOwned => response(
            StatusCode::FORBIDDEN,
            "you are not authorized to add this agent — you did not create it",
            Some("agent_not_owned"),
        ),
        ConversationMemberMutation::SelfTarget => response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "leaving a channel yourself is a different request",
            Some("self_target"),
        ),
        ConversationMemberMutation::TargetOutranksActor => response(
            StatusCode::FORBIDDEN,
            "this member holds a higher workspace role than you",
            Some("target_outranks_actor"),
        ),
        ConversationMemberMutation::ChannelArchived => response(
            StatusCode::CONFLICT,
            "cannot change members of an archived channel",
            Some("channel_archived"),
        ),
        ConversationMemberMutation::ChannelOwner => response(
            StatusCode::CONFLICT,
            "this member is the owner of a private channel",
            Some("channel_owner"),
        ),
        ConversationMemberMutation::LastHumanMember => response(
            StatusCode::CONFLICT,
            "this member is the last member of a private channel",
            Some("last_human_member"),
        ),
    }
}

async fn channel<A: ConversationMembershipAftermath>(
    outcome: ChannelMembershipOutcome,
    viewer: &UserId,
    conversation: &ConversationId,
    aftermath: &A,
) -> Response {
    match outcome {
        ChannelMembershipOutcome::Changed { channel, committed } => {
            let response = match (*channel).bind(viewer, conversation) {
                Ok(value) => super::channel_lifecycle::success(&value),
                Err(cause) => return failed("channel membership identity seal failed", &cause),
            };
            if let Some(changed) = committed.as_ref() {
                announce(aftermath, changed).await;
            }
            response
        }
        ChannelMembershipOutcome::Refused(ChannelMembershipRefusal::ChannelNotFound) => {
            response(StatusCode::NOT_FOUND, "channel not found", None)
        }
        ChannelMembershipOutcome::Refused(ChannelMembershipRefusal::Forbidden) => {
            response(StatusCode::FORBIDDEN, "forbidden", None)
        }
        ChannelMembershipOutcome::Refused(ChannelMembershipRefusal::NotMember) => response(
            StatusCode::FORBIDDEN,
            "not a member of this channel",
            Some("not_member"),
        ),
        ChannelMembershipOutcome::Refused(ChannelMembershipRefusal::ChannelArchived) => response(
            StatusCode::CONFLICT,
            "cannot leave an archived channel",
            Some("channel_archived"),
        ),
        ChannelMembershipOutcome::Refused(ChannelMembershipRefusal::ChannelOwner) => response(
            StatusCode::CONFLICT,
            "cannot leave as the owner of a private channel",
            Some("channel_owner"),
        ),
        ChannelMembershipOutcome::Refused(ChannelMembershipRefusal::LastHumanMember) => response(
            StatusCode::CONFLICT,
            "cannot leave as the last member of a private channel",
            Some("last_human_member"),
        ),
    }
}

async fn announce<A: ConversationMembershipAftermath>(
    aftermath: &A,
    changed: &ConversationMembershipChanged,
) {
    if let Err(cause) = aftermath.announce(changed).await {
        tracing::warn!(
            error_chain =
                crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
            conversation_id = changed.conversation_id.as_str(),
            "membership committed but post-commit announcement failed"
        );
    }
}

#[derive(Serialize)]
#[serde(tag = "kind")]
enum MemberDto {
    #[serde(rename = "agent")]
    Agent {
        agent_id: String,
        name: String,
        display_name: String,
        runtime: String,
    },
    #[serde(rename = "human")]
    Human {
        user_id: String,
        name: String,
        display_name: String,
        avatar_url: Option<String>,
    },
}
impl From<ConversationMember> for MemberDto {
    fn from(value: ConversationMember) -> Self {
        match value {
            ConversationMember::Agent {
                id,
                name,
                display_name,
                runtime,
            } => Self::Agent {
                agent_id: id.as_str().to_owned(),
                name,
                display_name,
                runtime,
            },
            ConversationMember::Human {
                id,
                name,
                display_name,
                avatar_storage_key,
                updated_at,
            } => Self::Human {
                avatar_url: avatar_storage_key
                    .filter(|key| !key.is_empty())
                    .map(|_| format!("/api/users/{}/avatar?v={updated_at}", id.as_str())),
                user_id: id.as_str().to_owned(),
                name,
                display_name,
            },
        }
    }
}
#[derive(Serialize)]
struct RosterDto {
    members: Vec<MemberDto>,
}
impl From<ConversationRoster> for RosterDto {
    fn from(value: ConversationRoster) -> Self {
        Self {
            members: value.members.into_iter().map(MemberDto::from).collect(),
        }
    }
}
#[derive(Serialize)]
struct RemovabilityDto {
    kind: &'static str,
    id: String,
    can_remove: bool,
}
#[derive(Serialize)]
struct RosterReadDto {
    members: Vec<MemberDto>,
    member_removability: Vec<RemovabilityDto>,
}
impl RosterReadDto {
    fn new(roster: ConversationRoster, removability: Vec<MemberRemovability>) -> Self {
        Self {
            members: roster.members.into_iter().map(MemberDto::from).collect(),
            member_removability: removability
                .into_iter()
                .map(|row| RemovabilityDto {
                    kind: if row.member_is_agent {
                        "agent"
                    } else {
                        "human"
                    },
                    id: row.id,
                    can_remove: row.can_remove,
                })
                .collect(),
        }
    }
}
#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'static str>,
}
fn response(status: StatusCode, message: &'static str, code: Option<&'static str>) -> Response {
    (
        status,
        Json(ErrorBody {
            error: message,
            code,
        }),
    )
        .into_response()
}
fn invalid() -> Response {
    response(StatusCode::UNPROCESSABLE_ENTITY, "invalid input", None)
}
fn failed(message: &'static str, cause: &(impl std::error::Error + 'static)) -> Response {
    tracing::error!(
        error_chain = crate::foundation::safe_log_field::safe_display(&(error::describe(cause))),
        message
    );
    internal()
}
fn capability_denied() -> Response {
    (StatusCode::FORBIDDEN, Json(serde_json::json!({"code":"capability_denied","capability":"manage_conversation_members","message":"missing 'manage_conversation_members' capability in this workspace","details":{"capability":"manage_conversation_members"}}))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        convert::Infallible,
        sync::{Arc, Mutex},
    };

    use axum::{
        body::{Body, to_bytes},
        http::{
            Method, Request,
            header::{AUTHORIZATION, CONTENT_TYPE},
        },
    };
    use silverhand_backend_core::{
        conversation_messaging::messaging::{AddressRefusal, DeliveryIntent, ResolvedStream},
        foundation::{identity::DaemonInstanceId, persistence::PersistenceFailure},
        host_runtime::runtime_credential::{RuntimeCustody, RuntimeGeneration, RuntimeScope},
    };
    use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;
    use sqlx::{Connection, Executor};
    use tower::ServiceExt;

    #[test]
    fn handle_normalization_matches_the_ascii_stored_name_envelope() {
        assert_eq!(
            NormalizedMemberHandle::try_from("  @Alice-1  ".to_owned())
                .unwrap()
                .as_str(),
            "Alice-1"
        );
        assert!(NormalizedMemberHandle::try_from("@@alice".to_owned()).is_err());
        assert!(NormalizedMemberHandle::try_from("1alice".to_owned()).is_err());
    }

    #[derive(Clone, Copy)]
    struct Clock;
    impl EnrollmentClock for Clock {
        fn now_unix_millis(&self) -> i64 {
            100
        }
    }

    #[derive(Clone, Default)]
    struct Effects {
        announcements: Arc<Mutex<Vec<ConversationMembershipChanged>>>,
        stops: Arc<Mutex<Vec<(ConversationId, AgentId)>>>,
    }
    impl ConversationMembershipAftermath for Effects {
        type Error = Infallible;
        async fn announce(
            &self,
            changed: &ConversationMembershipChanged,
        ) -> Result<(), Self::Error> {
            self.announcements.lock().unwrap().push(changed.clone());
            Ok(())
        }
    }
    impl MembershipRuntimeStop for Effects {
        type Error = Infallible;
        async fn stop(
            &self,
            conversation: &ConversationId,
            agent: &AgentId,
        ) -> Result<(), Self::Error> {
            self.stops
                .lock()
                .unwrap()
                .push((conversation.clone(), agent.clone()));
            Ok(())
        }
    }

    async fn route_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("membership-routes.sqlite");
        let mut db = crate::current_schema::create_current_schema(&path).await;
        db.execute("INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('owner','owner','Owner',1,1),('member','member','Member',1,1),('target','target','Target',1,1),('outside','outside','Outside',1,1)").await.unwrap();
        db.execute("INSERT INTO hosts(id,name,kind,created_at) VALUES('host','host','local',1)")
            .await
            .unwrap();
        db.execute("INSERT INTO agents(id,name,runtime,host_id,created_at,updated_at,created_by_user_id,display_name,session_scope) VALUES('caller','caller','codex','host',1,1,'owner','Caller','agent'),('peer','peer','codex','host',1,1,'owner','Peer','conversation')").await.unwrap();
        db.execute("INSERT INTO workspaces(id,name,name_key,created_at,updated_at,owner_user_id) VALUES('workspace','Workspace','workspace',1,1,'owner')").await.unwrap();
        db.execute("INSERT INTO workspace_users(workspace_id,user_id,joined_at,role) VALUES('workspace','owner',1,'owner'),('workspace','member',2,'member'),('workspace','target',3,'member')").await.unwrap();
        db.execute("INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES('workspace','caller',1),('workspace','peer',2)").await.unwrap();
        db.execute("INSERT INTO workspace_agent_capabilities(workspace_id,agent_id,capability) VALUES('workspace','caller','manage_conversation_members')").await.unwrap();
        db.execute("INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,created_at,updated_at,visibility) VALUES('public','workspace','workspace','open','Public','public','idle',1,1,'public')").await.unwrap();
        db.execute("INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,endpoint_a_kind,endpoint_a_id,endpoint_b_kind,endpoint_b_id,title,status,created_at,updated_at,visibility) VALUES('caller-pair',NULL,'direct_pair','fixed','agent','caller','human','owner','','idle',1,1,'private'),('foreign-pair',NULL,'direct_pair','fixed','agent','peer','human','target','','idle',1,1,'private')").await.unwrap();
        db.execute("INSERT INTO conversation_users(conversation_id,user_id,joined_at) VALUES('public','owner',1),('public','member',2)").await.unwrap();
        db.execute("INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES('public','caller',1),('public','peer',2)").await.unwrap();
        db.close().await.unwrap();
        (dir, path)
    }

    async fn public_call(method: Method, path: &str, viewer: &str, body: Option<&str>) -> Response {
        let (_dir, database) = route_fixture().await;
        let store = MessagingStore::open(&database).await.unwrap();
        let app = public_router(store, Clock, Effects::default())
            .layer(Extension(UserId::parse(viewer).unwrap()));
        let mut request = Request::builder().method(method).uri(path);
        let payload = body.unwrap_or_default();
        if body.is_some() {
            request = request.header(CONTENT_TYPE, "application/json");
        }
        app.oneshot(request.body(Body::from(payload.to_owned())).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn all_six_public_membership_methods_reach_real_store_success_contracts() {
        let cases = [
            (
                Method::GET,
                "/api/conversations/public/members",
                "owner",
                None,
            ),
            (
                Method::POST,
                "/api/conversations/public/members",
                "owner",
                Some(r#"{"kind":"human","user_id":"target"}"#),
            ),
            (Method::POST, "/api/channels/public/join", "target", None),
            (Method::POST, "/api/channels/public/leave", "member", None),
            (
                Method::DELETE,
                "/api/conversations/public/members/agents/peer",
                "owner",
                None,
            ),
            (
                Method::DELETE,
                "/api/conversations/public/members/users/member",
                "owner",
                None,
            ),
        ];
        for (method, path, viewer, body) in cases {
            let response = public_call(method.clone(), path, viewer, body).await;
            assert_eq!(response.status(), StatusCode::OK, "{method} {path}");
        }
    }

    #[tokio::test]
    async fn six_public_methods_pin_their_primary_missing_container_refusal() {
        let cases = [
            (Method::GET, "/api/conversations/missing/members", None),
            (
                Method::POST,
                "/api/conversations/missing/members",
                Some(r#"{"kind":"human","user_id":"target"}"#),
            ),
            (Method::POST, "/api/channels/missing/join", None),
            (Method::POST, "/api/channels/missing/leave", None),
            (
                Method::DELETE,
                "/api/conversations/missing/members/agents/peer",
                None,
            ),
            (
                Method::DELETE,
                "/api/conversations/missing/members/users/member",
                None,
            ),
        ];
        for (method, path, body) in cases {
            let response = public_call(method.clone(), path, "owner", body).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {path}");
        }
    }

    #[tokio::test]
    async fn agent_remove_classifies_fixed_and_closed_before_nonparticipant_visibility() {
        let fixed = public_call(
            Method::DELETE,
            "/api/conversations/foreign-pair/members/agents/peer",
            "owner",
            None,
        )
        .await;
        assert_eq!(fixed.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let fixed_json: serde_json::Value =
            serde_json::from_slice(&to_bytes(fixed.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(fixed_json["code"], "conversation_member_remove_unsupported");

        let (_dir, database) = route_fixture().await;
        let mut db = sqlx::SqliteConnection::connect(&format!("sqlite://{}", database.display()))
            .await
            .unwrap();
        db.execute("UPDATE conversations SET closed_at=50 WHERE id='public'")
            .await
            .unwrap();
        db.execute(
            "DELETE FROM conversation_users WHERE conversation_id='public' AND user_id='owner'",
        )
        .await
        .unwrap();
        db.close().await.unwrap();
        let store = MessagingStore::open(&database).await.unwrap();
        let response = public_router(store, Clock, Effects::default())
            .layer(Extension(UserId::parse("owner").unwrap()))
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri("/api/conversations/public/members/agents/peer")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let json: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(json["code"], "conversation_closed");
    }

    #[tokio::test]
    async fn archived_public_join_is_403_with_durable_and_effect_zero() {
        let (_dir, database) = route_fixture().await;
        let mut db = sqlx::SqliteConnection::connect(&format!("sqlite://{}", database.display()))
            .await
            .unwrap();
        db.execute("UPDATE conversations SET archived_at=50 WHERE id='public'")
            .await
            .unwrap();
        db.close().await.unwrap();
        let effects = Effects::default();
        let store = MessagingStore::open(&database).await.unwrap();
        let app = public_router(store, Clock, effects.clone())
            .layer(Extension(UserId::parse("target").unwrap()));
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/channels/public/join")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(effects.announcements.lock().unwrap().is_empty());
        let mut db = sqlx::SqliteConnection::connect(&format!("sqlite://{}", database.display()))
            .await
            .unwrap();
        for query in [
            "SELECT COUNT(*) FROM conversation_users WHERE conversation_id='public' AND user_id='target'",
            "SELECT COUNT(*) FROM channel_attention_preferences WHERE conversation_id='public' AND user_id='target'",
            "SELECT COUNT(*) FROM conversation_reads WHERE conversation_id='public' AND user_id='target'",
        ] {
            assert_eq!(
                sqlx::query_scalar::<_, i64>(query)
                    .fetch_one(&mut db)
                    .await
                    .unwrap(),
                0
            );
        }
    }

    #[tokio::test]
    async fn mixed_kind_handle_ambiguity_is_a_typed_route_contract() {
        let (_dir, database) = route_fixture().await;
        let mut db = sqlx::SqliteConnection::connect(&format!("sqlite://{}", database.display()))
            .await
            .unwrap();
        db.execute("DROP TRIGGER agents_name_unique_against_users_insert")
            .await
            .unwrap();
        db.execute("INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('same-human','same','Same Human',1,1)").await.unwrap();
        db.execute("INSERT INTO agents(id,name,runtime,host_id,created_at,updated_at,created_by_user_id,display_name,session_scope) VALUES('same-agent','same','codex','host',1,1,'owner','Same Agent','agent')").await.unwrap();
        db.execute("INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES('workspace','same-agent',1)").await.unwrap();
        db.close().await.unwrap();
        let store = MessagingStore::open(&database).await.unwrap();
        let response = public_router(store, Clock, Effects::default())
            .layer(Extension(UserId::parse("owner").unwrap()))
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/conversations/public/members")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"handle":"same"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let json: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(json["code"], "handle_ambiguous");
        assert_eq!(
            json["candidates"],
            serde_json::json!([
                {"kind":"agent","id":"same-agent","display_name":"Same Agent","handle":"@same"},
                {"kind":"human","id":"same-human","display_name":"Same Human","handle":"@same"}
            ])
        );
    }

    #[derive(Clone)]
    struct Target {
        outcome: Result<ResolvedStream, AddressRefusal>,
        calls: Arc<Mutex<Vec<(AgentId, DirectPairAllocation, DeliveryIntent)>>>,
    }
    impl TargetAddress for Target {
        type Error = Infallible;
        async fn resolve(
            &self,
            agent: &AgentId,
            _target: &str,
            allocation: DirectPairAllocation,
            intent: DeliveryIntent,
        ) -> Result<Result<ResolvedStream, AddressRefusal>, Self::Error> {
            self.calls
                .lock()
                .unwrap()
                .push((agent.clone(), allocation, intent));
            Ok(self.outcome.clone())
        }
    }

    #[derive(Clone)]
    struct PinnedAuthority {
        expected: AgentId,
        seen: Arc<Mutex<Vec<AgentId>>>,
    }
    impl crate::host_runtime::runtime_credential::AuthenticateRuntimeCredential for PinnedAuthority {
        async fn authenticate_runtime(
            &self,
            _bearer: crate::host_runtime::runtime_credential::RuntimeBearer,
            route_agent: AgentId,
        ) -> Result<Option<RuntimePrincipal>, PersistenceFailure> {
            self.seen.lock().unwrap().push(route_agent.clone());
            if route_agent != self.expected {
                return Ok(None);
            }
            Ok(Some(RuntimePrincipal::for_scope(
                RuntimeScope::AgentSlot { agent: route_agent },
                RuntimeCustody::new(
                    RuntimeGeneration::new(1).unwrap(),
                    DaemonInstanceId::parse("daemon").unwrap(),
                ),
            )))
        }
    }

    async fn internal_call(path_agent: &str, target: Target) -> Response {
        internal_call_handle(path_agent, target, "target").await
    }

    async fn internal_call_handle(path_agent: &str, target: Target, handle: &str) -> Response {
        let (_dir, database) = route_fixture().await;
        let store = MessagingStore::open(&database).await.unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let app = crate::host_runtime::runtime_credential::require_runtime_credential(
            internal_router(store, target, Clock, Effects::default()),
            PinnedAuthority {
                expected: AgentId::parse("caller").unwrap(),
                seen,
            },
        );
        app.oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/internal/agents/{path_agent}/members"))
                .header(AUTHORIZATION, "Bearer test-secret")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r##"{{"handle":"{handle}","target":"#Workspace/Public"}}"##,
                )))
                .unwrap(),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn internal_member_add_uses_runtime_self_binding_and_explicit_read_intent() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let target = Target {
            outcome: Ok(ResolvedStream {
                conversation: ConversationId::parse("public").unwrap(),
                thread_root: None,
            }),
            calls: calls.clone(),
        };
        assert_eq!(
            internal_call("other", target.clone()).await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert!(
            calls.lock().unwrap().is_empty(),
            "mismatched aid never reaches target authority"
        );
        assert_eq!(
            internal_call("caller", target).await.status(),
            StatusCode::OK
        );
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[(
                AgentId::parse("caller").unwrap(),
                DirectPairAllocation::Forbidden,
                DeliveryIntent::Read
            )]
        );
    }

    #[tokio::test]
    async fn direct_pair_endpoint_reaches_domain_unsupported_while_nonendpoint_is_capability_denied()
     {
        let endpoint = Target {
            outcome: Ok(ResolvedStream {
                conversation: ConversationId::parse("caller-pair").unwrap(),
                thread_root: None,
            }),
            calls: Arc::new(Mutex::new(Vec::new())),
        };
        let response = internal_call("caller", endpoint).await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let json: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(json["code"], "conversation_member_add_unsupported");

        let foreign = Target {
            outcome: Ok(ResolvedStream {
                conversation: ConversationId::parse("foreign-pair").unwrap(),
                thread_root: None,
            }),
            calls: Arc::new(Mutex::new(Vec::new())),
        };
        let response = internal_call("caller", foreign).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let json: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(json["code"], "capability_denied");
    }

    #[tokio::test]
    async fn agent_route_places_existing_workspace_human_and_agent_but_never_enrolls_human() {
        let public = || Target {
            outcome: Ok(ResolvedStream {
                conversation: ConversationId::parse("public").unwrap(),
                thread_root: None,
            }),
            calls: Arc::new(Mutex::new(Vec::new())),
        };
        assert_eq!(
            internal_call_handle("caller", public(), "target")
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            internal_call_handle("caller", public(), "peer")
                .await
                .status(),
            StatusCode::OK
        );
        let response = internal_call_handle("caller", public(), "outside").await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let json: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(json["code"], "workspace_membership_required");
    }

    #[tokio::test]
    async fn internal_target_refusals_and_thread_suffix_use_the_canonical_wire() {
        for (refusal, status, code) in [
            (
                AddressRefusal::Malformed,
                StatusCode::BAD_REQUEST,
                "target_malformed",
            ),
            (
                AddressRefusal::SelfPair,
                StatusCode::BAD_REQUEST,
                "self_pair_not_addressable",
            ),
            (
                AddressRefusal::Unresolved,
                StatusCode::NOT_FOUND,
                "target_unresolved",
            ),
            (
                AddressRefusal::ThreadNotInTarget,
                StatusCode::NOT_FOUND,
                "thread_not_in_target",
            ),
        ] {
            let response = internal_call(
                "caller",
                Target {
                    outcome: Err(refusal),
                    calls: Arc::new(Mutex::new(Vec::new())),
                },
            )
            .await;
            assert_eq!(response.status(), status);
            let json: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            assert_eq!(json["code"], code);
        }
        let target = Target { outcome: Ok(ResolvedStream { conversation: ConversationId::parse("public").unwrap(), thread_root: Some(silverhand_backend_core::conversation_messaging::messaging::ThreadRootId::parse("root").unwrap()) }), calls: Arc::new(Mutex::new(Vec::new())) };
        let response = internal_call("caller", target).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let json: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(json["code"], "thread_suffix_not_supported");
        assert_eq!(
            json["error"],
            "this operation addresses a conversation, not a reply cluster"
        );
    }

    #[tokio::test]
    async fn member_body_identity_arms_are_enforced_by_the_route() {
        for body in [
            r#"{"kind":"agent","user_id":"target"}"#,
            r#"{"kind":"human","agent_id":"peer"}"#,
            r#"["agent","peer"]"#,
            r#"{"kind":"agent","agent_id":"","handle":"target"}"#,
        ] {
            let response = public_call(
                Method::POST,
                "/api/conversations/public/members",
                "owner",
                Some(body),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{body}"
            );
        }
        let response = public_call(
            Method::POST,
            "/api/conversations/public/members",
            "owner",
            Some(r#"{"kind":"agent","agent_id":"peer"}"#),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn removal_target_errors_are_rendered_by_the_actual_route() {
        for (path, expected_status, expected_error) in [
            (
                "/api/conversations/caller-pair/members/users/target",
                StatusCode::UNPROCESSABLE_ENTITY,
                "human members can only be removed from channels",
            ),
            (
                "/api/conversations/public/members/agents/absent",
                StatusCode::NOT_FOUND,
                "agent not found",
            ),
        ] {
            let response = public_call(Method::DELETE, path, "owner", None).await;
            assert_eq!(response.status(), expected_status, "{path}");
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            assert_eq!(body["error"], expected_error, "{path}");
        }
    }
}
