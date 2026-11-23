//! Durable daemon-event facts and the transaction boundary that admits them.
//!
//! Daemon frames are not replayable after a successful settlement/finalization.
//! The ports here therefore commit each durable fact as one root unit of work;
//! process-local cache, live projection and coordinator effects run only after
//! that commit and never compensate it.

use std::future::Future;

use thiserror::Error;

use crate::{
    conversation_messaging::runtime_delivery::{
        DeliveryLeaseOwner, InboxDeliveryId, InboxSettlementDisposition,
    },
    foundation::identity::{AgentId, ConversationId, UserId},
    foundation::persistence::PersistenceFailure,
    host_runtime::runtime_credential::{RuntimeEventTime, RuntimePrincipal},
};

/// One daemon event admission fact: the authenticated host and its exact live
/// runtime custody are inseparable after the persistence boundary answers.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RuntimeEventAdmission {
    host_id: String,
    principal: RuntimePrincipal,
}

impl RuntimeEventAdmission {
    pub fn new(host_id: String, principal: RuntimePrincipal) -> Self {
        Self { host_id, principal }
    }

    pub fn host_id(&self) -> &str {
        &self.host_id
    }

    pub fn principal(&self) -> &RuntimePrincipal {
        &self.principal
    }
}

/// What a daemon last observed a runtime seat to be doing.
///
/// The persisted token and this enum are one bijection, so both halves live here:
/// an adapter that re-stated the inverse would be a second authority nothing forces
/// to agree with [`RuntimeObservedStatus::as_str`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeObservedStatus {
    Idle,
    Starting,
    Active,
    Crashed,
}

impl RuntimeObservedStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Starting => "starting",
            Self::Active => "active",
            Self::Crashed => "crashed",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, UnknownRuntimeObservedStatus> {
        match raw {
            "idle" => Ok(Self::Idle),
            "starting" => Ok(Self::Starting),
            "active" => Ok(Self::Active),
            "crashed" => Ok(Self::Crashed),
            _ => Err(UnknownRuntimeObservedStatus),
        }
    }
}

/// A stored value outside the closed set is unreadable, not a fifth status.
///
/// It must not decay into a default: every consumer of this vocabulary decides
/// whether a seat is running, and a token nobody wrote cannot be answered with "not
/// running" — that would turn corrupt data into a live liveness verdict.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
#[error("runtime observed status is outside the closed set")]
pub struct UnknownRuntimeObservedStatus;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeActivityPayload {
    None,
    Text {
        text: String,
    },
    ToolCall {
        name: String,
        input_json: String,
        /// Runtime-independent tool description; opaque JSON to the backend.
        tool_json: Option<String>,
        tool_call_id: Option<String>,
    },
    ToolOutput {
        name: Option<String>,
        tool_call_id: Option<String>,
        outcome: String,
    },
    ApiResponse {
        status: String,
        message: String,
    },
    Error {
        message: String,
        code: Option<String>,
        fatal: Option<bool>,
    },
    ParsedEventDropped {
        parsed_kind: String,
        encoded_bytes: i64,
        reason: String,
    },
    MessageReceived {
        count: usize,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeActivityKind {
    SessionInit,
    Thinking,
    Text,
    ToolCall,
    ToolOutput,
    TurnEnd,
    CompactionStarted,
    CompactionFinished,
    ApiResponse,
    Error,
    ParsedEventDropped,
    MessageStale,
    MessageReceived,
}

impl RuntimeActivityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionInit => "session_init",
            Self::Thinking => "thinking",
            Self::Text => "text",
            Self::ToolCall => "tool_call",
            Self::ToolOutput => "tool_output",
            Self::TurnEnd => "turn_end",
            Self::CompactionStarted => "compaction_started",
            Self::CompactionFinished => "compaction_finished",
            Self::ApiResponse => "api_response",
            Self::Error => "error",
            Self::ParsedEventDropped => "parsed_event_dropped",
            Self::MessageStale => "message_stale",
            Self::MessageReceived => "message_received",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeActivityFact {
    conversation: ConversationId,
    agent: AgentId,
    kind: RuntimeActivityKind,
    payload: RuntimeActivityPayload,
    created_at: RuntimeEventTime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpsertRuntimeTextActivity {
    admission: RuntimeEventAdmission,
    event: RuntimeActivityFact,
    previous_event_id: Option<i64>,
    presence_at: RuntimeEventTime,
}

impl UpsertRuntimeTextActivity {
    pub fn new(
        admission: RuntimeEventAdmission,
        event: RuntimeActivityFact,
        previous_event_id: Option<i64>,
        presence_at: RuntimeEventTime,
    ) -> Self {
        Self {
            admission,
            event,
            previous_event_id,
            presence_at,
        }
    }

    pub fn admission(&self) -> &RuntimeEventAdmission {
        &self.admission
    }
    pub fn event(&self) -> &RuntimeActivityFact {
        &self.event
    }
    pub fn previous_event_id(&self) -> Option<i64> {
        self.previous_event_id
    }
    pub fn presence_at(&self) -> RuntimeEventTime {
        self.presence_at
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeActivityCommitResult {
    id: i64,
    presence_viewers: Vec<UserId>,
}

impl RuntimeActivityCommitResult {
    pub fn new(id: i64, presence_viewers: Vec<UserId>) -> Self {
        Self {
            id,
            presence_viewers,
        }
    }
    pub fn id(&self) -> i64 {
        self.id
    }
    pub fn presence_viewers(&self) -> &[UserId] {
        &self.presence_viewers
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefineRuntimeToolCallInput {
    admission: RuntimeEventAdmission,
    conversation: ConversationId,
    agent: AgentId,
    tool_call_id: String,
    input_json: String,
    tool_json: Option<String>,
    observed_at: RuntimeEventTime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefinedRuntimeActivity {
    id: i64,
    payload_json: String,
    created_at: RuntimeEventTime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeToolCallInputCommitResult {
    refined: Option<RefinedRuntimeActivity>,
    presence_viewers: Vec<UserId>,
}

impl RuntimeToolCallInputCommitResult {
    pub fn new(refined: Option<RefinedRuntimeActivity>, presence_viewers: Vec<UserId>) -> Self {
        Self {
            refined,
            presence_viewers,
        }
    }

    pub fn refined(&self) -> Option<&RefinedRuntimeActivity> {
        self.refined.as_ref()
    }

    pub fn presence_viewers(&self) -> &[UserId] {
        &self.presence_viewers
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StampRuntimePresence {
    admission: RuntimeEventAdmission,
    agent: AgentId,
    observed_at: RuntimeEventTime,
}

impl StampRuntimePresence {
    pub fn new(
        admission: RuntimeEventAdmission,
        agent: AgentId,
        observed_at: RuntimeEventTime,
    ) -> Self {
        Self {
            admission,
            agent,
            observed_at,
        }
    }

    pub fn admission(&self) -> &RuntimeEventAdmission {
        &self.admission
    }

    pub fn agent(&self) -> &AgentId {
        &self.agent
    }

    pub fn observed_at(&self) -> RuntimeEventTime {
        self.observed_at
    }
}

impl RefinedRuntimeActivity {
    pub fn new(id: i64, payload_json: String, created_at: RuntimeEventTime) -> Self {
        Self {
            id,
            payload_json,
            created_at,
        }
    }

    pub fn id(&self) -> i64 {
        self.id
    }
    pub fn payload_json(&self) -> &str {
        &self.payload_json
    }
    pub fn created_at(&self) -> RuntimeEventTime {
        self.created_at
    }
}

impl RefineRuntimeToolCallInput {
    pub fn new(
        admission: RuntimeEventAdmission,
        conversation: ConversationId,
        agent: AgentId,
        tool_call_id: String,
        input_json: String,
        tool_json: Option<String>,
        observed_at: RuntimeEventTime,
    ) -> Self {
        Self {
            admission,
            conversation,
            agent,
            tool_call_id,
            input_json,
            tool_json,
            observed_at,
        }
    }

    pub fn admission(&self) -> &RuntimeEventAdmission {
        &self.admission
    }
    pub fn conversation(&self) -> &ConversationId {
        &self.conversation
    }
    pub fn agent(&self) -> &AgentId {
        &self.agent
    }
    pub fn tool_call_id(&self) -> &str {
        &self.tool_call_id
    }
    pub fn input_json(&self) -> &str {
        &self.input_json
    }
    pub fn tool_json(&self) -> Option<&str> {
        self.tool_json.as_deref()
    }
    pub fn observed_at(&self) -> RuntimeEventTime {
        self.observed_at
    }
}

impl RuntimeActivityFact {
    pub fn new(
        conversation: ConversationId,
        agent: AgentId,
        kind: RuntimeActivityKind,
        payload: RuntimeActivityPayload,
        created_at: RuntimeEventTime,
    ) -> Self {
        Self {
            conversation,
            agent,
            kind,
            payload,
            created_at,
        }
    }

    pub fn conversation(&self) -> &ConversationId {
        &self.conversation
    }
    pub fn agent(&self) -> &AgentId {
        &self.agent
    }
    pub fn kind(&self) -> RuntimeActivityKind {
        self.kind
    }
    pub fn payload(&self) -> &RuntimeActivityPayload {
        &self.payload
    }
    pub fn created_at(&self) -> RuntimeEventTime {
        self.created_at
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitRuntimeExit {
    admission: RuntimeEventAdmission,
    origin_conversation: ConversationId,
    status: RuntimeObservedStatus,
    clear_cli_session_id: bool,
    observed_at: RuntimeEventTime,
    pending_activity: Option<RuntimeActivityFact>,
}

impl CommitRuntimeExit {
    pub fn new(
        admission: RuntimeEventAdmission,
        origin_conversation: ConversationId,
        status: RuntimeObservedStatus,
        clear_cli_session_id: bool,
        observed_at: RuntimeEventTime,
        pending_activity: Option<RuntimeActivityFact>,
    ) -> Self {
        Self {
            admission,
            origin_conversation,
            status,
            clear_cli_session_id,
            observed_at,
            pending_activity,
        }
    }

    pub fn admission(&self) -> &RuntimeEventAdmission {
        &self.admission
    }
    pub fn status(&self) -> RuntimeObservedStatus {
        self.status
    }
    pub fn origin_conversation(&self) -> &ConversationId {
        &self.origin_conversation
    }
    pub fn clear_cli_session_id(&self) -> bool {
        self.clear_cli_session_id
    }
    pub fn observed_at(&self) -> RuntimeEventTime {
        self.observed_at
    }
    pub fn pending_activity(&self) -> Option<&RuntimeActivityFact> {
        self.pending_activity.as_ref()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeExitDisposition {
    Applied,
    Stale,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeExitCommitResult {
    disposition: RuntimeExitDisposition,
    activity_id: Option<i64>,
    status_targets: Vec<RuntimeStatusTarget>,
}

impl RuntimeExitCommitResult {
    pub fn new(
        disposition: RuntimeExitDisposition,
        activity_id: Option<i64>,
        status_targets: Vec<RuntimeStatusTarget>,
    ) -> Self {
        Self {
            disposition,
            activity_id,
            status_targets,
        }
    }

    pub fn disposition(&self) -> RuntimeExitDisposition {
        self.disposition
    }
    pub fn activity_id(&self) -> Option<i64> {
        self.activity_id
    }
    pub fn status_targets(&self) -> &[RuntimeStatusTarget] {
        &self.status_targets
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeStatusTarget {
    conversation: ConversationId,
    carries_origin_detail: bool,
}

impl RuntimeStatusTarget {
    pub fn new(conversation: ConversationId, carries_origin_detail: bool) -> Self {
        Self {
            conversation,
            carries_origin_detail,
        }
    }

    pub fn conversation(&self) -> &ConversationId {
        &self.conversation
    }

    pub fn carries_origin_detail(&self) -> bool {
        self.carries_origin_detail
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeStatusCommitResult {
    targets: Vec<RuntimeStatusTarget>,
}

impl RuntimeStatusCommitResult {
    pub fn new(targets: Vec<RuntimeStatusTarget>) -> Self {
        Self { targets }
    }

    pub fn targets(&self) -> &[RuntimeStatusTarget] {
        &self.targets
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeConsumedCommitResult {
    disposition: InboxSettlementDisposition,
    activity_id: Option<i64>,
    presence_viewers: Vec<UserId>,
}

impl RuntimeConsumedCommitResult {
    pub fn new(
        disposition: InboxSettlementDisposition,
        activity_id: Option<i64>,
        presence_viewers: Vec<UserId>,
    ) -> Self {
        Self {
            disposition,
            activity_id,
            presence_viewers,
        }
    }

    pub fn disposition(&self) -> InboxSettlementDisposition {
        self.disposition
    }
    pub fn activity_id(&self) -> Option<i64> {
        self.activity_id
    }
    pub fn presence_viewers(&self) -> &[UserId] {
        &self.presence_viewers
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitRuntimeConsumed {
    admission: RuntimeEventAdmission,
    conversation: ConversationId,
    owner: DeliveryLeaseOwner,
    delivery_ids: Vec<InboxDeliveryId>,
    observed_at: RuntimeEventTime,
    presence_at: RuntimeEventTime,
}

impl CommitRuntimeConsumed {
    pub fn new(
        admission: RuntimeEventAdmission,
        conversation: ConversationId,
        owner: DeliveryLeaseOwner,
        delivery_ids: Vec<InboxDeliveryId>,
        observed_at: RuntimeEventTime,
        presence_at: RuntimeEventTime,
    ) -> Self {
        Self {
            admission,
            conversation,
            owner,
            delivery_ids,
            observed_at,
            presence_at,
        }
    }

    pub fn host_id(&self) -> &str {
        self.admission.host_id()
    }
    pub fn principal(&self) -> &RuntimePrincipal {
        self.admission.principal()
    }
    pub fn admission(&self) -> &RuntimeEventAdmission {
        &self.admission
    }
    pub fn conversation(&self) -> &ConversationId {
        &self.conversation
    }
    pub fn owner(&self) -> &DeliveryLeaseOwner {
        &self.owner
    }
    pub fn delivery_ids(&self) -> &[InboxDeliveryId] {
        &self.delivery_ids
    }
    pub fn observed_at(&self) -> RuntimeEventTime {
        self.observed_at
    }
    pub fn presence_at(&self) -> RuntimeEventTime {
        self.presence_at
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectRuntimeStatus {
    admission: RuntimeEventAdmission,
    origin_conversation: ConversationId,
    status: RuntimeObservedStatus,
    cli_session_id: Option<String>,
    observed_at: RuntimeEventTime,
}

impl ProjectRuntimeStatus {
    pub fn new(
        admission: RuntimeEventAdmission,
        origin_conversation: ConversationId,
        status: RuntimeObservedStatus,
        cli_session_id: Option<String>,
        observed_at: RuntimeEventTime,
    ) -> Self {
        Self {
            admission,
            origin_conversation,
            status,
            cli_session_id,
            observed_at,
        }
    }

    pub fn admission(&self) -> &RuntimeEventAdmission {
        &self.admission
    }
    pub fn status(&self) -> RuntimeObservedStatus {
        self.status
    }
    pub fn origin_conversation(&self) -> &ConversationId {
        &self.origin_conversation
    }
    pub fn cli_session_id(&self) -> Option<&str> {
        self.cli_session_id.as_deref()
    }
    pub fn observed_at(&self) -> RuntimeEventTime {
        self.observed_at
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitRuntimeSessionInit {
    admission: RuntimeEventAdmission,
    conversation: ConversationId,
    agent: AgentId,
    cli_session_id: String,
    observed_at: RuntimeEventTime,
    presence_at: RuntimeEventTime,
}

impl CommitRuntimeSessionInit {
    pub fn new(
        admission: RuntimeEventAdmission,
        conversation: ConversationId,
        agent: AgentId,
        cli_session_id: String,
        observed_at: RuntimeEventTime,
        presence_at: RuntimeEventTime,
    ) -> Self {
        Self {
            admission,
            conversation,
            agent,
            cli_session_id,
            observed_at,
            presence_at,
        }
    }

    pub fn admission(&self) -> &RuntimeEventAdmission {
        &self.admission
    }
    pub fn conversation(&self) -> &ConversationId {
        &self.conversation
    }
    pub fn agent(&self) -> &AgentId {
        &self.agent
    }
    pub fn cli_session_id(&self) -> &str {
        &self.cli_session_id
    }
    pub fn observed_at(&self) -> RuntimeEventTime {
        self.observed_at
    }
    pub fn presence_at(&self) -> RuntimeEventTime {
        self.presence_at
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeSessionInitCommitResult {
    activity_id: i64,
    presence_viewers: Vec<UserId>,
    status_targets: Vec<RuntimeStatusTarget>,
}

impl RuntimeSessionInitCommitResult {
    pub fn new(
        activity_id: i64,
        presence_viewers: Vec<UserId>,
        status_targets: Vec<RuntimeStatusTarget>,
    ) -> Self {
        Self {
            activity_id,
            presence_viewers,
            status_targets,
        }
    }
    pub fn activity_id(&self) -> i64 {
        self.activity_id
    }
    pub fn presence_viewers(&self) -> &[UserId] {
        &self.presence_viewers
    }
    pub fn status_targets(&self) -> &[RuntimeStatusTarget] {
        &self.status_targets
    }
}

pub trait DaemonEventDurablePort: Clone + Send + Sync + 'static {
    /// Current custody and placement targets in one read snapshot. Used for
    /// shared run-state projection; it grants no access to origin content.
    fn runtime_activity_targets(
        &self,
        admission: RuntimeEventAdmission,
        origin: ConversationId,
    ) -> impl Future<Output = Result<Option<Vec<RuntimeStatusTarget>>, PersistenceFailure>> + Send;

    fn current_admission(
        &self,
        host_id: String,
        scope: crate::host_runtime::runtime_credential::RuntimeScope,
    ) -> impl Future<Output = Result<Option<RuntimeEventAdmission>, PersistenceFailure>> + Send;

    /// Whether the current custody still has an unclaimed `turn` delivery in
    /// `conversation`. Pending `observe` context alone is not owed a run.
    fn has_pending_turn(
        &self,
        admission: RuntimeEventAdmission,
        conversation: ConversationId,
    ) -> impl Future<Output = Result<bool, PersistenceFailure>> + Send;

    fn commit_exit(
        &self,
        event: CommitRuntimeExit,
    ) -> impl Future<Output = Result<RuntimeExitCommitResult, PersistenceFailure>> + Send;

    fn commit_consumed(
        &self,
        event: CommitRuntimeConsumed,
    ) -> impl Future<Output = Result<RuntimeConsumedCommitResult, PersistenceFailure>> + Send;

    fn commit_session_init(
        &self,
        event: CommitRuntimeSessionInit,
    ) -> impl Future<Output = Result<Option<RuntimeSessionInitCommitResult>, PersistenceFailure>> + Send;

    fn project_status(
        &self,
        event: ProjectRuntimeStatus,
    ) -> impl Future<Output = Result<Option<RuntimeStatusCommitResult>, PersistenceFailure>> + Send;

    fn record_activity(
        &self,
        admission: RuntimeEventAdmission,
        event: RuntimeActivityFact,
        presence_at: RuntimeEventTime,
    ) -> impl Future<Output = Result<Option<RuntimeActivityCommitResult>, PersistenceFailure>> + Send;

    fn upsert_text_activity(
        &self,
        event: UpsertRuntimeTextActivity,
    ) -> impl Future<Output = Result<Option<RuntimeActivityCommitResult>, PersistenceFailure>> + Send;

    fn refine_tool_call_input(
        &self,
        event: RefineRuntimeToolCallInput,
    ) -> impl Future<Output = Result<Option<RuntimeToolCallInputCommitResult>, PersistenceFailure>> + Send;

    fn stamp_presence(
        &self,
        event: StampRuntimePresence,
    ) -> impl Future<Output = Result<Option<Vec<UserId>>, PersistenceFailure>> + Send;

    /// Records an `agent_start_failed` notice for the agent's owner. Returns the
    /// notified owner, or `None` when an unread notice for the agent already exists.
    fn record_start_failure(
        &self,
        event: RecordRuntimeStartFailure,
    ) -> impl Future<Output = Result<Option<UserId>, PersistenceFailure>> + Send;
}

/// A daemon refused to start an agent's runtime for a reason its owner can act on.
/// `message` is display text the daemon composed from a fixed template.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordRuntimeStartFailure {
    pub agent: AgentId,
    pub conversation: ConversationId,
    pub reason: &'static str,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observed_status_uses_the_exact_persisted_tokens_in_both_directions() {
        for (status, token) in [
            (RuntimeObservedStatus::Idle, "idle"),
            (RuntimeObservedStatus::Starting, "starting"),
            (RuntimeObservedStatus::Active, "active"),
            (RuntimeObservedStatus::Crashed, "crashed"),
        ] {
            assert_eq!(status.as_str(), token);
            assert_eq!(RuntimeObservedStatus::parse(token), Ok(status));
        }
    }

    #[test]
    fn an_unknown_token_is_unreadable_rather_than_a_fifth_status() {
        assert_eq!(
            RuntimeObservedStatus::parse("terminated"),
            Err(UnknownRuntimeObservedStatus)
        );
        assert_eq!(
            RuntimeObservedStatus::parse(""),
            Err(UnknownRuntimeObservedStatus)
        );
    }
}
