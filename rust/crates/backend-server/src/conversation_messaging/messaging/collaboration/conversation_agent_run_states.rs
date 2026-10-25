//! Authenticated HTTP projection of the daemon owner's live run-state map.
//!
//! This capability intentionally joins two independent authorities at the
//! boundary: SQLite decides whether the viewer may see the requested
//! conversation, while `RuntimeEventSettlement` supplies the ephemeral
//! process-memory snapshot. No runtime state is reconstructed from storage.

use std::{error::Error, fmt, sync::Arc};

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::json;
use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::channel_lifecycle::{
        ChannelDetailReadPort, ChannelDetailRefusal,
    },
    foundation::identity::{AgentId, ConversationId, UserId},
};

use crate::foundation::http_error::json_error as error;
use crate::{
    application_orchestration::daemon_delivery::DaemonDeliveryRuntime,
    host_runtime::runtime_event_settlement::{
        AgentRunStateWire as DaemonAgentRunState,
        ConversationAgentRunStateSnapshot as DaemonAgentRunStateSnapshot, DaemonRuntimeEventError,
    },
};

impl<C, D, H> DaemonDeliveryRuntime<C, D, H>
where
    C: silverhand_backend_core::host_runtime::runtime_credential::RuntimeCustodyPort,
    D: silverhand_backend_core::conversation_messaging::runtime_delivery::RuntimeDispatchReadPort
        + silverhand_backend_core::conversation_messaging::runtime_delivery::RuntimeInboxCustodyPort
        + silverhand_backend_core::host_runtime::runtime_event::DaemonEventDurablePort,
    H: silverhand_backend_core::host_runtime::custody::HostCredentialLookup,
{
    pub fn agent_run_states_router<A>(&self, admission: A) -> Router
    where
        A: ChannelDetailReadPort + Send + Sync + 'static,
    {
        router(admission, self.clone())
    }
}

pub const CHANNEL_AGENT_RUN_STATES_PATH: &str = "/api/channels/{id}/agent-run-states";

/// A live owner snapshot source. Implementations must return one atomic
/// snapshot, not independently sampled per-agent reads.
pub trait ConversationAgentRunStateSource {
    type Error: Error + Send + Sync + 'static;

    fn snapshot(
        &self,
        conversation: &ConversationId,
    ) -> impl std::future::Future<Output = Result<Vec<AgentRunStateProjection>, Self::Error>> + Send;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentRunStateProjection {
    source_agent: AgentId,
    source_conversation: ConversationId,
    agent: AgentId,
    conversation: ConversationId,
    wire: DaemonAgentRunState,
}

impl AgentRunStateProjection {
    pub fn agent(&self) -> &AgentId {
        &self.agent
    }

    pub fn conversation(&self) -> &ConversationId {
        &self.conversation
    }

    pub fn run_id(&self) -> &str {
        &self.wire.run_id
    }

    pub fn bind(
        self,
        requested: &ConversationId,
    ) -> Result<BoundAgentRunState, AgentRunStateIdentityMismatch> {
        if &self.source_conversation != requested
            || &self.conversation != requested
            || self.source_agent != self.agent
        {
            return Err(AgentRunStateIdentityMismatch);
        }
        Ok(BoundAgentRunState(self))
    }

    fn from_daemon(value: DaemonAgentRunStateSnapshot) -> Result<Self, RunStateSnapshotFailure> {
        let DaemonAgentRunStateSnapshot {
            source_agent,
            source_conversation,
            state: value,
        } = value;
        let agent = AgentId::parse(value.agent_id.as_str())
            .map_err(|_| RunStateSnapshotFailure::malformed_identity())?;
        let conversation = ConversationId::parse(value.conversation_id.as_str())
            .map_err(|_| RunStateSnapshotFailure::malformed_identity())?;
        Ok(Self {
            source_agent,
            source_conversation,
            agent,
            conversation,
            wire: value,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundAgentRunState(AgentRunStateProjection);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentRunStateIdentityMismatch;

impl fmt::Display for AgentRunStateIdentityMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("live agent run state does not belong to the requested conversation")
    }
}

impl Error for AgentRunStateIdentityMismatch {}

#[derive(Debug)]
pub struct RunStateSnapshotFailure {
    cause: RunStateSnapshotFailureCause,
}

#[derive(Debug)]
enum RunStateSnapshotFailureCause {
    Owner(DaemonRuntimeEventError),
    MalformedIdentity,
}

impl RunStateSnapshotFailure {
    fn owner(source: DaemonRuntimeEventError) -> Self {
        Self {
            cause: RunStateSnapshotFailureCause::Owner(source),
        }
    }

    fn malformed_identity() -> Self {
        Self {
            cause: RunStateSnapshotFailureCause::MalformedIdentity,
        }
    }
}

impl fmt::Display for RunStateSnapshotFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match &self.cause {
            RunStateSnapshotFailureCause::Owner(_) => "live agent run-state owner snapshot failed",
            RunStateSnapshotFailureCause::MalformedIdentity => {
                "live agent run-state identity is malformed"
            }
        })
    }
}

impl Error for RunStateSnapshotFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.cause {
            RunStateSnapshotFailureCause::Owner(source) => Some(source),
            RunStateSnapshotFailureCause::MalformedIdentity => None,
        }
    }
}

impl<C, D, H> ConversationAgentRunStateSource for DaemonDeliveryRuntime<C, D, H>
where
    D: silverhand_backend_core::host_runtime::runtime_event::DaemonEventDurablePort,
    C: Sync,
    H: Sync,
{
    type Error = RunStateSnapshotFailure;

    async fn snapshot(
        &self,
        conversation: &ConversationId,
    ) -> Result<Vec<AgentRunStateProjection>, Self::Error> {
        self.conversation_agent_run_states(conversation)
            .await
            .map_err(RunStateSnapshotFailure::owner)?
            .into_iter()
            .map(AgentRunStateProjection::from_daemon)
            .collect()
    }
}

struct RunStatesState<A, S> {
    admission: Arc<A>,
    snapshot: Arc<S>,
}

impl<A, S> Clone for RunStatesState<A, S> {
    fn clone(&self) -> Self {
        Self {
            admission: Arc::clone(&self.admission),
            snapshot: Arc::clone(&self.snapshot),
        }
    }
}

/// Build the standalone component router. Authentication/mounting belongs to
/// the backend-app composition lane and is intentionally absent here.
pub fn router<A, S>(admission: A, snapshot: S) -> Router
where
    A: ChannelDetailReadPort + Send + Sync + 'static,
    S: ConversationAgentRunStateSource + Send + Sync + 'static,
{
    Router::new()
        .route(CHANNEL_AGENT_RUN_STATES_PATH, get(read::<A, S>))
        .with_state(RunStatesState {
            admission: Arc::new(admission),
            snapshot: Arc::new(snapshot),
        })
}

async fn read<A, S>(
    State(state): State<RunStatesState<A, S>>,
    principal: Option<Extension<UserId>>,
    Path(raw_conversation): Path<String>,
) -> Response
where
    A: ChannelDetailReadPort,
    S: ConversationAgentRunStateSource,
{
    let Some(Extension(viewer)) = principal else {
        tracing::error!("conversation-agent-run-states route reached without human principal");
        return internal();
    };
    let conversation = match ConversationId::parse(raw_conversation) {
        Ok(value) => value,
        Err(_) => return error(StatusCode::BAD_REQUEST, "channel id required"),
    };

    match state.admission.detail(&viewer, &conversation).await {
        Ok(Ok(fact)) => {
            if let Err(failure) = fact.bind(&viewer, &conversation) {
                tracing::error!(
                    error_chain = crate::foundation::safe_log_field::safe_display(
                        &(crate::foundation::error::describe(&failure))
                    ),
                    "conversation-agent-run-states admission identity seal failed"
                );
                return internal();
            }
        }
        Ok(Err(ChannelDetailRefusal::NotFound)) => {
            return error(StatusCode::NOT_FOUND, "channel not found");
        }
        Ok(Err(ChannelDetailRefusal::Forbidden)) => {
            return error(StatusCode::FORBIDDEN, "forbidden");
        }
        Err(failure) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&failure))
                ),
                "conversation-agent-run-states admission failed"
            );
            return internal();
        }
    }

    let states = match state.snapshot.snapshot(&conversation).await {
        Ok(states) => states,
        Err(failure) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&failure))
                ),
                "conversation-agent-run-states owner snapshot failed"
            );
            return internal();
        }
    };
    let bound = match states
        .into_iter()
        .map(|state| state.bind(&conversation))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(states) => states,
        Err(failure) => {
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&failure))
                ),
                "conversation-agent-run-states snapshot identity seal failed"
            );
            return internal();
        }
    };
    success(&bound)
}

fn success(states: &[BoundAgentRunState]) -> Response {
    Json(json!({
        "states": states
            .iter()
            .map(|state| &state.0.wire)
            .collect::<Vec<_>>()
    }))
    .into_response()
}

fn internal() -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

#[cfg(test)]
mod tests;
