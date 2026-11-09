//! Typed settlement authority for daemon runtime state, activity, and diagnostics.

use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{
    Deserialize, Serialize,
    de::{DeserializeOwned, Error as _},
};
use serde_json::{Value, json};
use silverhand_backend_core::{
    foundation::identity::{AgentId, ConversationId, DaemonInstanceId, HostId, UserId},
    foundation::persistence::PersistenceFailure,
    host_runtime::runtime_credential::{
        RuntimeCustody, RuntimeEventTime, RuntimeGeneration, RuntimePrincipal, RuntimeScope,
    },
    host_runtime::runtime_event::{
        CommitRuntimeExit, CommitRuntimeSessionInit, DaemonEventDurablePort, ProjectRuntimeStatus,
        RefineRuntimeToolCallInput, RefinedRuntimeActivity, RuntimeActivityFact,
        RuntimeActivityKind, RuntimeActivityPayload, RuntimeEventAdmission,
        RuntimeExitCommitResult, RuntimeExitDisposition, RuntimeObservedStatus,
        RuntimeStatusTarget, StampRuntimePresence, UpsertRuntimeTextActivity,
    },
};

use crate::host_runtime::host_control::status_seed::{
    AttachedStatusCustody, CustodyUnavailable, HostRuntimeActivity,
};

/// A transport-neutral, source-preserving failure from an already-authorized
/// live projection. Durable truth has already committed when this error is
/// retained for a drain directive.
#[derive(Debug)]
pub(crate) struct LiveProjectionFailure(Box<dyn Error + Send + Sync>);

impl LiveProjectionFailure {
    pub(crate) fn from_source(source: impl Error + Send + Sync + 'static) -> Self {
        Self(Box::new(source))
    }
}

impl fmt::Display for LiveProjectionFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("daemon runtime live projection failed")
    }
}

impl Error for LiveProjectionFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.0.as_ref())
    }
}

#[derive(Clone, Debug)]
pub(crate) enum ConversationRuntimeProjectionFact {
    AgentActivity {
        conversation: ConversationId,
        state: AgentRunStateWire,
    },
    AgentInitTimeout {
        conversation: ConversationId,
        agent: AgentId,
        reason: InitTimeoutReason,
        elapsed_ms: i64,
        timeout_ms: i64,
        retrying: bool,
        retry_attempt: i64,
        max_retries: i64,
        cooldown_until: Option<i64>,
    },
    CompactionStale {
        conversation: ConversationId,
        agent: AgentId,
        elapsed_ms: i64,
    },
    AgentEvent {
        scope: RuntimeScope,
        conversation: ConversationId,
        agent: AgentId,
        event: ParsedEventWire,
    },
    Activity {
        scope: RuntimeScope,
        id: i64,
        fact: RuntimeActivityFact,
    },
    RefinedToolCall {
        scope: RuntimeScope,
        conversation: ConversationId,
        agent: AgentId,
        activity: RefinedRuntimeActivity,
    },
}

pub(crate) trait ConversationRuntimeProjectionPort: Send + Sync {
    fn publish(&self, fact: ConversationRuntimeProjectionFact)
    -> Result<(), LiveProjectionFailure>;
}

pub(crate) trait AgentPresenceEffectPort: Send + Sync {
    fn publish(&self, viewers: &[UserId], agent: &AgentId) -> Result<(), LiveProjectionFailure>;
}

#[derive(Clone, Debug)]
pub(crate) enum RuntimeStatusProjectionUpdate {
    Active {
        cli_session_id: Option<String>,
    },
    StartingRetry {
        retry_attempt: i64,
        max_retries: i64,
        timeout_reason: Option<InitTimeoutReason>,
    },
    CrashedCooldown {
        cooldown_until: i64,
        attempts_exhausted: i64,
    },
    Exit {
        status: RuntimeObservedStatus,
        code: Option<i64>,
        intentional: bool,
        clear_cli_session_id: bool,
        run_state: Option<Box<AgentRunStateWire>>,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimeStatusProjectionFact {
    pub(crate) targets: Vec<RuntimeStatusTarget>,
    pub(crate) agent: AgentId,
    pub(crate) update: RuntimeStatusProjectionUpdate,
}

pub(crate) trait RuntimeStatusProjectionPort: Send + Sync {
    fn publish_host_connection_changed(
        &self,
        owner: &UserId,
        host: &HostId,
    ) -> Result<(), LiveProjectionFailure>;

    fn publish(&self, fact: RuntimeStatusProjectionFact) -> Result<(), LiveProjectionFailure>;

    fn publish_start_failure(&self, owner: &UserId) -> Result<(), LiveProjectionFailure>;
}

#[derive(Debug)]
pub(crate) enum RuntimeEventDirective {
    None,
    Drain {
        conversation: ConversationId,
        agent: AgentId,
        post_commit_failure: Option<LiveProjectionFailure>,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimeExitPostCommit {
    conversation: ConversationId,
    agent: AgentId,
    status_targets: Vec<RuntimeStatusTarget>,
    status: RuntimeObservedStatus,
    code: Option<i64>,
    intentional: bool,
    clear_cli_session_id: bool,
    run_state: Option<AgentRunStateWire>,
    shared: bool,
    respawn: bool,
}

impl RuntimeExitPostCommit {
    pub(crate) fn should_respawn(&self) -> bool {
        self.respawn
    }
    pub(crate) fn conversation(&self) -> &ConversationId {
        &self.conversation
    }
    pub(crate) fn agent(&self) -> &AgentId {
        &self.agent
    }
}

#[derive(Clone)]
pub(crate) struct RuntimeEventSettlement<P> {
    durable: P,
    conversation_projection: Arc<dyn ConversationRuntimeProjectionPort>,
    presence_effect: Arc<dyn AgentPresenceEffectPort>,
    status_projection: Arc<dyn RuntimeStatusProjectionPort>,
    state: Arc<Mutex<RuntimeOwnerState>>,
}

impl<P> RuntimeEventSettlement<P> {
    pub(crate) fn host_runtime_activity(&self) -> HostRuntimeActivitySource {
        HostRuntimeActivitySource {
            state: Arc::clone(&self.state),
        }
    }

    /// A cloneable handle onto the run-state projection for the workspace-delete
    /// bulk `conversation_closed` terminalization — the SAME run map +
    /// conversation projection, never a second store.
    pub(crate) fn conversation_run_terminalizer(&self) -> ConversationRunTerminalizer {
        ConversationRunTerminalizer {
            state: Arc::clone(&self.state),
            conversation_projection: Arc::clone(&self.conversation_projection),
        }
    }

    pub(crate) fn publish_start_failure(
        &self,
        owner: &UserId,
    ) -> Result<(), DaemonRuntimeEventError> {
        self.status_projection
            .publish_start_failure(owner)
            .map_err(DaemonRuntimeEventError::from)
    }

    pub(crate) fn publish_host_connection_changed(
        &self,
        owner: &silverhand_backend_core::foundation::identity::UserId,
        host: &HostId,
    ) -> Result<(), DaemonRuntimeEventError> {
        self.status_projection
            .publish_host_connection_changed(owner, host)
            .map_err(DaemonRuntimeEventError::from)
    }
    /// Copy the complete live owner map under its single mutex, then release
    /// the lock before conversation filtering, verification and ordering. This
    /// gives callers one atomic owner snapshot without lengthening the daemon
    /// event writer's critical section.
    fn candidate_run_states(
        &self,
        conversation: &ConversationId,
    ) -> Result<
        Vec<(RuntimeEventAdmission, ConversationAgentRunStateSnapshot)>,
        DaemonRuntimeEventError,
    > {
        let snapshot = {
            let state = self
                .state
                .lock()
                .map_err(|_| DaemonRuntimeEventError::StatePoisoned)?;
            state
                .runtimes
                .iter()
                .filter_map(|(admission, entry)| {
                    entry
                        .run
                        .as_ref()
                        .map(|run| (admission.clone(), run.clone()))
                })
                .collect::<Vec<_>>()
        };
        let mut selected = Vec::new();
        for (admission, run_state) in snapshot {
            let principal = admission.principal();
            let source_agent = principal.agent().clone();
            let source_conversation = match principal.source() {
                Some(source) => source.clone(),
                None => ConversationId::parse(run_state.conversation_id.clone())
                    .map_err(DaemonRuntimeEventError::source)?,
            };
            if run_state.agent_id != source_agent.as_str()
                || run_state.conversation_id != source_conversation.as_str()
            {
                return Err(DaemonRuntimeEventError::InvalidPayload);
            }
            if principal.source().is_some() && &source_conversation != conversation {
                continue;
            }
            selected.push((
                admission,
                ConversationAgentRunStateSnapshot {
                    source_agent,
                    source_conversation,
                    state: run_state,
                },
            ));
        }
        // TS exposes no ordering contract.  Rust refines this to a stable total
        // order without adding product meaning: visible identity first, then
        // hidden custody coordinates solely as collision tiebreakers.
        selected.sort_by(|(left_admission, left), (right_admission, right)| {
            (
                left.state.agent_id.as_str(),
                left.state.run_id.as_str(),
                left_admission.host_id(),
                left_admission.principal().custody().generation().as_i64(),
                left_admission
                    .principal()
                    .custody()
                    .daemon_instance()
                    .as_str(),
            )
                .cmp(&(
                    right.state.agent_id.as_str(),
                    right.state.run_id.as_str(),
                    right_admission.host_id(),
                    right_admission.principal().custody().generation().as_i64(),
                    right_admission
                        .principal()
                        .custody()
                        .daemon_instance()
                        .as_str(),
                ))
        });
        Ok(selected)
    }
}

#[derive(Default)]
struct RuntimeOwnerState {
    runtimes: HashMap<RuntimeEventAdmission, RuntimeEntry>,
    host_activity: HashMap<String, HostActivityState>,
}

/// Process-local facts for one admitted runtime generation; they live and die together.
#[derive(Default)]
struct RuntimeEntry {
    run: Option<AgentRunStateWire>,
    pending_text: Option<PendingTextActivity>,
    timeout_reason: Option<InitTimeoutReason>,
    init_failed: bool,
    fatal: bool,
    clear_cli_session_on_exit: bool,
}

impl RuntimeEntry {
    fn is_empty(&self) -> bool {
        self.run.is_none()
            && self.pending_text.is_none()
            && self.timeout_reason.is_none()
            && !self.init_failed
            && !self.fatal
            && !self.clear_cli_session_on_exit
    }
}

impl RuntimeOwnerState {
    fn update(&mut self, admission: &RuntimeEventAdmission, apply: impl FnOnce(&mut RuntimeEntry)) {
        let entry = self.runtimes.entry(admission.clone()).or_default();
        apply(entry);
        if entry.is_empty() {
            self.runtimes.remove(admission);
        }
    }

    fn get(&self, admission: &RuntimeEventAdmission) -> Option<&RuntimeEntry> {
        self.runtimes.get(admission)
    }
}

struct HostActivityState {
    daemon_instance: DaemonInstanceId,
    connection_id: String,
    active_agents: HashSet<AgentId>,
}

#[derive(Clone)]
pub(crate) struct HostRuntimeActivitySource {
    state: Arc<Mutex<RuntimeOwnerState>>,
}

impl HostRuntimeActivity for HostRuntimeActivitySource {
    fn active_agents(
        &self,
        host: &HostId,
        custody: &AttachedStatusCustody,
    ) -> Result<Option<HashSet<AgentId>>, CustodyUnavailable> {
        let state = self.state.lock().map_err(|_| CustodyUnavailable)?;
        Ok(state
            .host_activity
            .get(host.as_str())
            .filter(|activity| {
                activity.daemon_instance == custody.daemon_instance
                    && activity.connection_id == custody.connection_id
            })
            .map(|activity| activity.active_agents.clone()))
    }
}

/// A cloneable, durable-generic-free handle onto the run-state projection for the
/// bulk `conversation_closed` terminalization the workspace-delete aftermath drives.
/// It shares the ONE in-memory run map + the ONE conversation
/// projection with the runtime-event owner, so it is the SAME authority — never a
/// second run-state store.
#[derive(Clone)]
pub struct ConversationRunTerminalizer {
    state: Arc<Mutex<RuntimeOwnerState>>,
    conversation_projection: Arc<dyn ConversationRuntimeProjectionPort>,
}

/// The public best-effort failure of the workspace-delete `conversation_closed`
/// terminalization: the run map lock was poisoned, or at least one
/// per-run `agent_activity` publish failed. The aftermath observes it and logs; it
/// never rewrites the 204 or aborts the sibling teardown legs.
#[derive(Debug)]
pub struct ConversationRunTerminalizeError;

impl fmt::Display for ConversationRunTerminalizeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("workspace-delete conversation_closed terminalization failed")
    }
}

impl Error for ConversationRunTerminalizeError {}

impl ConversationRunTerminalizer {
    /// Terminalize every non-terminal ConversationSlot run whose source conversation
    /// is in `conversations` to `conversation_closed`, publishing EXACTLY ONE
    /// `agent_activity` per terminalized run. AgentSlot runs (no source conversation)
    /// are EXCLUDED. An already-terminal run is left untouched (no duplicate event).
    /// Publishing runs after the lock is released; a per-run publish failure is
    /// observable and does not abort the others.
    pub fn terminalize_conversations_closed(
        &self,
        conversations: &std::collections::HashSet<ConversationId>,
    ) -> Result<(), ConversationRunTerminalizeError> {
        let mut announcements: Vec<(ConversationId, AgentRunStateWire)> = Vec::new();
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| ConversationRunTerminalizeError)?;
            let now = presence_now();
            for (admission, entry) in state.runtimes.iter_mut() {
                let Some(run_state) = entry.run.as_mut() else {
                    continue;
                };
                let Some(source) = admission.principal().source() else {
                    continue;
                };
                if !conversations.contains(source) {
                    continue;
                }
                if run_state.terminal_reason.is_some() {
                    continue;
                }
                run_state.terminal_reason = Some(AgentRunTerminalReason::ConversationClosed);
                run_state.status = AgentRunStatus::Idle;
                run_state.updated_at = serde_json::Number::from(now);
                announcements.push((source.clone(), run_state.clone()));
            }
        }
        let mut failed = false;
        for (conversation, run_state) in announcements {
            if let Err(cause) = self.conversation_projection.publish(
                ConversationRuntimeProjectionFact::AgentActivity {
                    conversation,
                    state: run_state,
                },
            ) {
                tracing::warn!(
                    error = crate::foundation::safe_log_field::safe_display(&cause),
                    "workspace-delete conversation_closed projection publish failed"
                );
                failed = true;
            }
        }
        if failed {
            Err(ConversationRunTerminalizeError)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone)]
struct PendingTextActivity {
    conversation: ConversationId,
    agent: AgentId,
    kind: RuntimeActivityKind,
    text: String,
    omitted_chars: usize,
    created_at: RuntimeEventTime,
    event_id: Option<i64>,
}

const TEXT_ACTIVITY_MAX_CHARS: usize = 16 * 1024;

impl<P> RuntimeEventSettlement<P>
where
    P: DaemonEventDurablePort,
{
    pub(crate) async fn conversation_run_states(
        &self,
        conversation: &ConversationId,
    ) -> Result<Vec<ConversationAgentRunStateSnapshot>, DaemonRuntimeEventError> {
        let candidates = self.candidate_run_states(conversation)?;
        let mut selected = Vec::new();
        for (admission, mut snapshot) in candidates {
            if admission.principal().source().is_some() {
                selected.push(snapshot);
                continue;
            }
            let Some(targets) = self
                .durable
                .runtime_activity_targets(admission.clone(), snapshot.source_conversation.clone())
                .await?
            else {
                continue;
            };
            let Some(target) = targets.iter().find(|t| t.conversation() == conversation) else {
                continue;
            };
            snapshot.state = snapshot.state.shared_projection(target.conversation());
            // This coordinate comes from the matched placement authority, not
            // from the caller or the runtime's mutable presentation focus.
            snapshot.source_conversation = target.conversation().clone();
            selected.push(snapshot);
        }
        Ok(selected)
    }

    pub(crate) fn new(
        durable: P,
        conversation_projection: Arc<dyn ConversationRuntimeProjectionPort>,
        presence_effect: Arc<dyn AgentPresenceEffectPort>,
        status_projection: Arc<dyn RuntimeStatusProjectionPort>,
    ) -> Self {
        Self {
            durable,
            conversation_projection,
            presence_effect,
            status_projection,
            state: Arc::new(Mutex::new(RuntimeOwnerState::default())),
        }
    }

    pub(crate) fn durable(&self) -> &P {
        &self.durable
    }

    pub(crate) async fn hydrate(
        &self,
        host_id: &str,
        daemon_instance: &DaemonInstanceId,
        connection_id: &str,
        active_agents: HashSet<AgentId>,
        runtimes: Vec<HydratedRuntime>,
    ) -> Result<(), DaemonRuntimeEventError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| DaemonRuntimeEventError::StatePoisoned)?;
        let hydrated_daemons = runtimes
            .iter()
            .map(|runtime| runtime.principal.custody().daemon_instance().clone())
            .collect::<HashSet<_>>();
        state.runtimes.retain(|admission, _| {
            admission.host_id() != host_id
                || !hydrated_daemons.contains(admission.principal().custody().daemon_instance())
        });
        for runtime in runtimes {
            if let Some(run_state) = runtime.run_state {
                state
                    .runtimes
                    .entry(RuntimeEventAdmission::new(
                        host_id.to_owned(),
                        runtime.principal,
                    ))
                    .or_default()
                    .run = Some(run_state);
            }
        }
        state.host_activity.insert(
            host_id.to_owned(),
            HostActivityState {
                daemon_instance: daemon_instance.clone(),
                connection_id: connection_id.to_owned(),
                active_agents,
            },
        );
        Ok(())
    }

    pub(crate) fn detach_daemon(
        &self,
        host_id: &str,
        daemon: &DaemonInstanceId,
        connection_id: &str,
    ) -> Result<(), DaemonRuntimeEventError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| DaemonRuntimeEventError::StatePoisoned)?;
        state.runtimes.retain(|admission, _| {
            admission.host_id() != host_id
                || admission.principal().custody().daemon_instance() != daemon
        });
        if state.host_activity.get(host_id).is_some_and(|activity| {
            activity.daemon_instance == *daemon && activity.connection_id == connection_id
        }) {
            state.host_activity.remove(host_id);
        }
        Ok(())
    }

    pub(crate) async fn handle(
        &self,
        host_id: &str,
        daemon: &DaemonInstanceId,
        connection_id: &str,
        event: RuntimeProductEvent,
    ) -> Result<RuntimeEventDirective, DaemonRuntimeEventError> {
        match event {
            RuntimeProductEvent::State(event) => {
                let admission = self
                    .current_admission(host_id, daemon, event.coordinate.scope()?)
                    .await?;
                if admission.principal().custody().generation().as_i64() != event.runtime_generation
                {
                    return Ok(RuntimeEventDirective::None);
                }
                let Some(targets) = self
                    .durable
                    .runtime_activity_targets(admission.clone(), event.coordinate.conversation()?)
                    .await?
                else {
                    return Ok(RuntimeEventDirective::None);
                };
                let shared = admission.principal().source().is_none();
                {
                    let mut state = self
                        .state
                        .lock()
                        .map_err(|_| DaemonRuntimeEventError::StatePoisoned)?;
                    if event.state.terminal_reason.is_none() {
                        state.update(&admission, |entry| {
                            entry.timeout_reason = None;
                            entry.init_failed = false;
                            entry.fatal = false;
                            entry.clear_cli_session_on_exit = false;
                        });
                    }
                    let Some(activity) = state.host_activity.get_mut(host_id).filter(|activity| {
                        activity.daemon_instance == *daemon
                            && activity.connection_id == connection_id
                    }) else {
                        return Ok(RuntimeEventDirective::None);
                    };
                    activity
                        .active_agents
                        .insert(admission.principal().agent().clone());
                    state.runtimes.entry(admission.clone()).or_default().run =
                        Some(event.state.clone());
                }
                let mut first_failure = None;
                for target in targets {
                    let projected = if shared {
                        event.state.shared_projection(target.conversation())
                    } else {
                        event.state.clone()
                    };
                    if let Err(error) = self.publish_conversation(
                        ConversationRuntimeProjectionFact::AgentActivity {
                            conversation: target.conversation().clone(),
                            state: projected,
                        },
                    ) && first_failure.is_none()
                    {
                        first_failure = Some(error);
                    }
                }
                // A turn that ended while `turn` deliveries are still queued behind
                // the batch it took is drained now; nothing else would wake it.
                let conversation = event.coordinate.conversation()?;
                let drain = event.state.terminal_reason == Some(AgentRunTerminalReason::TurnEnded)
                    && self
                        .durable
                        .has_pending_turn(admission, conversation.clone())
                        .await?;
                if drain {
                    return Ok(RuntimeEventDirective::Drain {
                        conversation,
                        agent: event.coordinate.agent()?,
                        post_commit_failure: first_failure,
                    });
                }
                if let Some(error) = first_failure {
                    return Err(error.into());
                }
                Ok(RuntimeEventDirective::None)
            }
            RuntimeProductEvent::Output(event) => {
                let _admission = self
                    .current_admission(host_id, daemon, event.coordinate.scope()?)
                    .await?;
                tracing::info!(
                    runtime_event = "runtime_output",
                    conversation_id = crate::foundation::safe_log_field::safe_display(
                        &(event.coordinate.conversation_id)
                    ),
                    agent_id = crate::foundation::safe_log_field::safe_display(
                        &(event.coordinate.agent_id)
                    ),
                    stream = crate::foundation::safe_log_field::safe_display(&(event.stream)),
                    chunk_bytes = event.chunk.len(),
                    observed_at = event.ts,
                    "daemon runtime output"
                );
                Ok(RuntimeEventDirective::None)
            }
            RuntimeProductEvent::ReadyForDrain(event) => {
                let admission = self
                    .admission_for_generation(
                        host_id,
                        daemon,
                        &event.coordinate,
                        event.runtime_generation,
                    )
                    .await?;
                let Some(projected) = self
                    .durable
                    .project_status(ProjectRuntimeStatus::new(
                        admission,
                        event.coordinate.conversation()?,
                        RuntimeObservedStatus::Active,
                        None,
                        RuntimeEventTime::from_unix_millis(event.ts),
                    ))
                    .await?
                else {
                    return Ok(RuntimeEventDirective::None);
                };
                let post_commit_failure = self
                    .publish_status(
                        projected.targets(),
                        &event.coordinate.agent()?,
                        RuntimeStatusProjectionUpdate::Active {
                            cli_session_id: None,
                        },
                    )
                    .err();
                Ok(RuntimeEventDirective::Drain {
                    conversation: event.coordinate.conversation()?,
                    agent: event.coordinate.agent()?,
                    post_commit_failure,
                })
            }
            RuntimeProductEvent::InitTimeout(event) => {
                let admission = self
                    .admission_for_generation(
                        host_id,
                        daemon,
                        &event.coordinate,
                        event.runtime_generation,
                    )
                    .await?;
                {
                    let mut state = self
                        .state
                        .lock()
                        .map_err(|_| DaemonRuntimeEventError::StatePoisoned)?;
                    state.update(&admission, |entry| {
                        entry.timeout_reason = Some(event.timeout_reason);
                        if !event.retrying {
                            entry.init_failed = true;
                        }
                    });
                }
                self.publish_conversation(ConversationRuntimeProjectionFact::AgentInitTimeout {
                    conversation: event.coordinate.conversation()?,
                    agent: event.coordinate.agent()?,
                    reason: event.timeout_reason,
                    elapsed_ms: event.elapsed_ms,
                    timeout_ms: event.timeout_ms,
                    retrying: event.retrying,
                    retry_attempt: event.retry_attempt,
                    max_retries: event.max_retries,
                    cooldown_until: event.cooldown_until,
                })?;
                Ok(RuntimeEventDirective::None)
            }
            RuntimeProductEvent::InitRetrying(event) => {
                let admission = self
                    .admission_for_generation(
                        host_id,
                        daemon,
                        &event.coordinate,
                        event.runtime_generation,
                    )
                    .await?;
                let timeout = self
                    .state
                    .lock()
                    .map_err(|_| DaemonRuntimeEventError::StatePoisoned)?
                    .get(&admission)
                    .and_then(|entry| entry.timeout_reason);
                if let Some(projected) = self
                    .durable
                    .project_status(ProjectRuntimeStatus::new(
                        admission,
                        event.coordinate.conversation()?,
                        RuntimeObservedStatus::Starting,
                        None,
                        RuntimeEventTime::from_unix_millis(event.ts),
                    ))
                    .await?
                {
                    self.publish_status(
                        projected.targets(),
                        &event.coordinate.agent()?,
                        RuntimeStatusProjectionUpdate::StartingRetry {
                            retry_attempt: event.retry_attempt,
                            max_retries: event.max_retries,
                            timeout_reason: timeout,
                        },
                    )?;
                }
                Ok(RuntimeEventDirective::None)
            }
            RuntimeProductEvent::InitCooldown(event) => {
                let admission = self
                    .admission_for_generation(
                        host_id,
                        daemon,
                        &event.coordinate,
                        event.runtime_generation,
                    )
                    .await?;
                {
                    let mut state = self
                        .state
                        .lock()
                        .map_err(|_| DaemonRuntimeEventError::StatePoisoned)?;
                    state.update(&admission, |entry| {
                        entry.timeout_reason = None;
                        entry.init_failed = true;
                    });
                }
                if let Some(projected) = self
                    .durable
                    .project_status(ProjectRuntimeStatus::new(
                        admission,
                        event.coordinate.conversation()?,
                        RuntimeObservedStatus::Crashed,
                        None,
                        RuntimeEventTime::from_unix_millis(event.ts),
                    ))
                    .await?
                {
                    self.publish_status(
                        projected.targets(),
                        &event.coordinate.agent()?,
                        RuntimeStatusProjectionUpdate::CrashedCooldown {
                            cooldown_until: event.cooldown_until,
                            attempts_exhausted: event.attempts_exhausted,
                        },
                    )?;
                }
                Ok(RuntimeEventDirective::None)
            }
            RuntimeProductEvent::CompactionStale(event) => {
                let _admission = self
                    .current_admission(host_id, daemon, event.coordinate.scope()?)
                    .await?;
                self.publish_conversation(ConversationRuntimeProjectionFact::CompactionStale {
                    conversation: event.coordinate.conversation()?,
                    agent: event.coordinate.agent()?,
                    elapsed_ms: event.elapsed_ms,
                })?;
                Ok(RuntimeEventDirective::None)
            }
            RuntimeProductEvent::OutputDropped(event) => {
                let _admission = self
                    .current_admission(host_id, daemon, event.coordinate.scope()?)
                    .await?;
                tracing::warn!(
                    runtime_event = "runtime_output_dropped",
                    conversation_id = crate::foundation::safe_log_field::safe_display(
                        &(event.coordinate.conversation_id)
                    ),
                    agent_id = crate::foundation::safe_log_field::safe_display(
                        &(event.coordinate.agent_id)
                    ),
                    dropped_count = event.dropped_count,
                    bytes_dropped = event.bytes_dropped,
                    since_ts = event.since_ts,
                    "daemon runtime output dropped"
                );
                Ok(RuntimeEventDirective::None)
            }
            RuntimeProductEvent::OutputPaused(event) => {
                let _admission = self
                    .current_admission(host_id, daemon, event.coordinate.scope()?)
                    .await?;
                tracing::info!(
                    runtime_event = "runtime_output_paused",
                    conversation_id = crate::foundation::safe_log_field::safe_display(
                        &(event.coordinate.conversation_id)
                    ),
                    agent_id = crate::foundation::safe_log_field::safe_display(
                        &(event.coordinate.agent_id)
                    ),
                    "daemon runtime output paused"
                );
                Ok(RuntimeEventDirective::None)
            }
            RuntimeProductEvent::OutputResumed(event) => {
                let _admission = self
                    .current_admission(host_id, daemon, event.coordinate.scope()?)
                    .await?;
                tracing::info!(
                    runtime_event = "runtime_output_resumed",
                    conversation_id = crate::foundation::safe_log_field::safe_display(
                        &(event.coordinate.conversation_id)
                    ),
                    agent_id = crate::foundation::safe_log_field::safe_display(
                        &(event.coordinate.agent_id)
                    ),
                    "daemon runtime output resumed"
                );
                Ok(RuntimeEventDirective::None)
            }
            RuntimeProductEvent::Parsed(event) => self.handle_parsed(host_id, daemon, event).await,
            RuntimeProductEvent::ParsedDropped(event) => {
                let admission = self
                    .current_admission(host_id, daemon, event.coordinate.scope()?)
                    .await?;
                let fact = RuntimeActivityFact::new(
                    event.coordinate.conversation()?,
                    event.coordinate.agent()?,
                    RuntimeActivityKind::ParsedEventDropped,
                    RuntimeActivityPayload::ParsedEventDropped {
                        parsed_kind: event.parsed_kind,
                        encoded_bytes: event.encoded_bytes,
                        reason: event.reason,
                    },
                    RuntimeEventTime::from_unix_millis(event.ts),
                );
                self.record_and_publish(admission, fact, None).await?;
                Ok(RuntimeEventDirective::None)
            }
            RuntimeProductEvent::MessageStale(event) => {
                let admission = self
                    .admission_for_generation(
                        host_id,
                        daemon,
                        &event.coordinate,
                        event.runtime_generation,
                    )
                    .await?;
                let fact = RuntimeActivityFact::new(
                    event.coordinate.conversation()?,
                    event.coordinate.agent()?,
                    RuntimeActivityKind::MessageStale,
                    RuntimeActivityPayload::None,
                    RuntimeEventTime::from_unix_millis(event.ts),
                );
                self.record_and_publish(admission, fact, None).await?;
                Ok(RuntimeEventDirective::None)
            }
        }
    }

    pub(crate) fn prepare_exit(
        &self,
        host_id: &str,
        principal: RuntimePrincipal,
        origin_conversation: ConversationId,
        code: Option<i64>,
        intentional: bool,
        observed_at: i64,
    ) -> Result<CommitRuntimeExit, DaemonRuntimeEventError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| DaemonRuntimeEventError::StatePoisoned)?;
        let admission = RuntimeEventAdmission::new(host_id.to_owned(), principal);
        state.update(&admission, |entry| entry.pending_text = None);
        let entry = state.get(&admission);
        let failed = entry.is_some_and(|entry| entry.init_failed || entry.fatal);
        let clear_cli_session_id = entry.is_some_and(|entry| entry.clear_cli_session_on_exit);
        let status = runtime_exit_status(failed, false, intentional, code);
        Ok(CommitRuntimeExit::new(
            admission,
            origin_conversation,
            status,
            clear_cli_session_id,
            RuntimeEventTime::from_unix_millis(observed_at),
            None,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn complete_consumed(
        &self,
        scope: RuntimeScope,
        conversation: &ConversationId,
        agent: &AgentId,
        presence_viewers: &[silverhand_backend_core::foundation::identity::UserId],
        observed_at: i64,
        count: usize,
        activity_id: i64,
    ) -> Result<(), DaemonRuntimeEventError> {
        let presence = self.publish_presence(presence_viewers, agent);
        let activity = self.publish_conversation(ConversationRuntimeProjectionFact::Activity {
            scope,
            id: activity_id,
            fact: RuntimeActivityFact::new(
                conversation.clone(),
                agent.clone(),
                RuntimeActivityKind::MessageReceived,
                RuntimeActivityPayload::MessageReceived { count },
                RuntimeEventTime::from_unix_millis(observed_at),
            ),
        });
        finish_live_effects([presence, activity]).map_err(Into::into)
    }

    pub(crate) fn complete_exit_state(
        &self,
        admission: &RuntimeEventAdmission,
        connection_id: &str,
        result: &RuntimeExitCommitResult,
        conversation: ConversationId,
        code: Option<i64>,
        intentional: bool,
    ) -> Result<Option<RuntimeExitPostCommit>, DaemonRuntimeEventError> {
        if result.disposition() != RuntimeExitDisposition::Applied {
            return Ok(None);
        }
        let (init_failed, fatal, clear_cli_session_id, run_state) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| DaemonRuntimeEventError::StatePoisoned)?;
            let entry = state.runtimes.remove(admission).unwrap_or_default();
            let init_failed = entry.init_failed;
            let fatal = entry.fatal;
            let clear_cli_session_id = entry.clear_cli_session_on_exit;
            let run_state = entry.run;
            if let Some(activity) =
                state
                    .host_activity
                    .get_mut(admission.host_id())
                    .filter(|activity| {
                        activity.daemon_instance
                            == *admission.principal().custody().daemon_instance()
                            && activity.connection_id == connection_id
                    })
            {
                activity.active_agents.remove(admission.principal().agent());
            }
            (init_failed, fatal, clear_cli_session_id, run_state)
        };
        let status = runtime_exit_status(init_failed, fatal, intentional, code);
        Ok(Some(RuntimeExitPostCommit {
            conversation,
            agent: admission.principal().agent().clone(),
            status_targets: result.status_targets().to_vec(),
            status,
            code,
            intentional,
            clear_cli_session_id,
            run_state,
            shared: admission.principal().source().is_none(),
            respawn: runtime_exit_should_respawn(init_failed, fatal, intentional),
        }))
    }

    pub(crate) fn publish_exit(
        &self,
        effect: &RuntimeExitPostCommit,
    ) -> Result<(), DaemonRuntimeEventError> {
        // A terminal shared run must reach the same targets as its live state,
        // otherwise other channels retain a ghost "thinking" until reconnect.
        let mut activity_failure = None;
        let mut status_run = effect.run_state.clone();
        if effect.shared
            && let Some(run) = &effect.run_state
        {
            let mut terminal = run.clone();
            terminal.status = if effect.status == RuntimeObservedStatus::Crashed {
                AgentRunStatus::Failed
            } else {
                AgentRunStatus::Idle
            };
            terminal.terminal_reason = Some(if effect.intentional {
                AgentRunTerminalReason::Stopped
            } else if effect.status == RuntimeObservedStatus::Crashed {
                AgentRunTerminalReason::Crashed
            } else {
                AgentRunTerminalReason::TurnEnded
            });
            terminal.updated_at = serde_json::Number::from(presence_now());
            status_run = Some(terminal.shared_projection(&effect.conversation));
            for target in &effect.status_targets {
                if let Err(error) =
                    self.publish_conversation(ConversationRuntimeProjectionFact::AgentActivity {
                        conversation: target.conversation().clone(),
                        state: terminal.shared_projection(target.conversation()),
                    })
                    && activity_failure.is_none()
                {
                    activity_failure = Some(error);
                }
            }
        }
        let status = self.publish_status(
            &effect.status_targets,
            &effect.agent,
            RuntimeStatusProjectionUpdate::Exit {
                status: effect.status,
                code: effect.code,
                intentional: effect.intentional,
                clear_cli_session_id: effect.clear_cli_session_id,
                run_state: status_run.map(Box::new),
            },
        );
        finish_live_effects([activity_failure.map_or(Ok(()), Err), status]).map_err(Into::into)
    }

    async fn handle_parsed(
        &self,
        host_id: &str,
        daemon: &DaemonInstanceId,
        event: RuntimeParsedEvent,
    ) -> Result<RuntimeEventDirective, DaemonRuntimeEventError> {
        let admission = self
            .admission_for_generation(host_id, daemon, &event.coordinate, event.runtime_generation)
            .await?;
        let parsed_kind = event.parsed.kind();
        if let ParsedEventWire::InputLifecycle {
            command_uuid,
            state,
        } = &event.parsed
        {
            tracing::info!(runtime_event = "runtime_parsed_event", parsed_kind, command_uuid, state = ?state, "runtime input lifecycle");
            return Ok(RuntimeEventDirective::None);
        }
        let sanitized = event.parsed.sanitized();
        debug_assert_eq!(sanitized.kind(), parsed_kind);
        let agent_event = ConversationRuntimeProjectionFact::AgentEvent {
            scope: event.coordinate.scope()?,
            conversation: event.coordinate.conversation()?,
            agent: event.coordinate.agent()?,
            event: sanitized,
        };
        if let ParsedEventWire::Error {
            code,
            fatal: Some(true),
            ..
        } = &event.parsed
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| DaemonRuntimeEventError::StatePoisoned)?;
            let clear = code
                .as_deref()
                .is_some_and(|code| matches!(code, "thread_not_found" | "session_archived"));
            state.update(&admission, |entry| {
                entry.fatal = true;
                entry.clear_cli_session_on_exit |= clear;
            });
        }
        if let ParsedEventWire::SessionInit { session_id } = &event.parsed {
            let conversation = event.coordinate.conversation()?;
            let agent = event.coordinate.agent()?;
            let committed = self
                .durable
                .commit_session_init(CommitRuntimeSessionInit::new(
                    admission.clone(),
                    conversation.clone(),
                    agent.clone(),
                    session_id.clone(),
                    RuntimeEventTime::from_unix_millis(event.ts),
                    RuntimeEventTime::from_unix_millis(presence_now()),
                ))
                .await?
                .ok_or(DaemonRuntimeEventError::StaleRuntime)?;
            let presence = self.publish_presence(committed.presence_viewers(), &agent);
            let agent_projection = self.publish_conversation(agent_event);
            let activity = self.publish_conversation(ConversationRuntimeProjectionFact::Activity {
                scope: admission.principal().scope(),
                id: committed.activity_id(),
                fact: RuntimeActivityFact::new(
                    conversation.clone(),
                    agent.clone(),
                    RuntimeActivityKind::SessionInit,
                    RuntimeActivityPayload::Text {
                        text: session_id.clone(),
                    },
                    RuntimeEventTime::from_unix_millis(event.ts),
                ),
            });
            let status = self.publish_status(
                committed.status_targets(),
                &agent,
                RuntimeStatusProjectionUpdate::Active {
                    cli_session_id: Some(session_id.clone()),
                },
            );
            let post_commit_failure =
                finish_live_effects([presence, agent_projection, activity, status]).err();
            return Ok(RuntimeEventDirective::Drain {
                conversation,
                agent,
                post_commit_failure,
            });
        }
        let directive = RuntimeEventDirective::None;
        let activity_projection = match &event.parsed {
            ParsedEventWire::Thinking { text } => {
                self.upsert_text_and_publish(
                    admission,
                    &event.coordinate,
                    RuntimeActivityKind::Thinking,
                    text,
                    event.ts,
                    agent_event.clone(),
                )
                .await
            }
            ParsedEventWire::Text { text } => {
                self.upsert_text_and_publish(
                    admission,
                    &event.coordinate,
                    RuntimeActivityKind::Text,
                    text,
                    event.ts,
                    agent_event.clone(),
                )
                .await
            }
            ParsedEventWire::ToolCallInput {
                tool_call_id,
                input,
            } => {
                self.clear_pending_text(&admission)?;
                let input = sanitize_json_value(input, 0);
                let Some(committed) = self
                    .durable
                    .refine_tool_call_input(RefineRuntimeToolCallInput::new(
                        admission.clone(),
                        event.coordinate.conversation()?,
                        event.coordinate.agent()?,
                        tool_call_id.clone(),
                        serde_json::to_string(&input)?,
                        RuntimeEventTime::from_unix_millis(presence_now()),
                    ))
                    .await?
                else {
                    return Err(DaemonRuntimeEventError::StaleRuntime);
                };
                let agent = event.coordinate.agent()?;
                let presence = self.publish_presence(committed.presence_viewers(), &agent);
                let agent_projection = self.publish_conversation(agent_event.clone());
                let activity = if let Some(refined) = committed.refined() {
                    self.publish_conversation(ConversationRuntimeProjectionFact::RefinedToolCall {
                        scope: admission.principal().scope(),
                        conversation: event.coordinate.conversation()?,
                        agent: event.coordinate.agent()?,
                        activity: refined.clone(),
                    })
                } else {
                    Ok(())
                };
                finish_live_effects([presence, agent_projection, activity]).map_err(Into::into)
            }
            _ => {
                self.clear_pending_text(&admission)?;
                if let Some(fact) = event.parsed.activity(&event.coordinate, event.ts)? {
                    self.record_and_publish(admission, fact, Some(agent_event.clone()))
                        .await
                } else {
                    let viewers = self
                        .durable
                        .stamp_presence(StampRuntimePresence::new(
                            admission,
                            event.coordinate.agent()?,
                            RuntimeEventTime::from_unix_millis(presence_now()),
                        ))
                        .await?
                        .ok_or(DaemonRuntimeEventError::StaleRuntime)?;
                    let presence = self.publish_presence(&viewers, &event.coordinate.agent()?);
                    let agent_projection = self.publish_conversation(agent_event.clone());
                    finish_live_effects([presence, agent_projection]).map_err(Into::into)
                }
            }
        };
        let live_failure = match activity_projection {
            Ok(()) => None,
            Err(DaemonRuntimeEventError::LiveProjection(cause)) => Some(cause),
            Err(cause) => return Err(cause),
        };
        if let Some(cause) = live_failure {
            return Err(DaemonRuntimeEventError::LiveProjection(cause));
        }
        Ok(directive)
    }

    fn clear_pending_text(
        &self,
        admission: &RuntimeEventAdmission,
    ) -> Result<(), DaemonRuntimeEventError> {
        self.state
            .lock()
            .map_err(|_| DaemonRuntimeEventError::StatePoisoned)?
            .update(admission, |entry| entry.pending_text = None);
        Ok(())
    }

    async fn upsert_text_and_publish(
        &self,
        admission: RuntimeEventAdmission,
        coordinate: &RuntimeCoordinate,
        kind: RuntimeActivityKind,
        fragment: &str,
        observed_at: i64,
        agent_event: ConversationRuntimeProjectionFact,
    ) -> Result<(), DaemonRuntimeEventError> {
        if fragment.is_empty() {
            let viewers = self
                .durable
                .stamp_presence(StampRuntimePresence::new(
                    admission,
                    coordinate.agent()?,
                    RuntimeEventTime::from_unix_millis(presence_now()),
                ))
                .await?
                .ok_or(DaemonRuntimeEventError::StaleRuntime)?;
            let presence = self.publish_presence(&viewers, &coordinate.agent()?);
            let agent_projection = self.publish_conversation(agent_event);
            return finish_live_effects([presence, agent_projection]).map_err(Into::into);
        }
        let (fact, previous_event_id) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| DaemonRuntimeEventError::StatePoisoned)?;
            let entry = state.runtimes.entry(admission.clone()).or_default();
            if entry
                .pending_text
                .as_ref()
                .is_some_and(|pending| pending.kind != kind)
            {
                entry.pending_text = None;
            }
            let pending = entry.pending_text.get_or_insert(PendingTextActivity {
                conversation: coordinate.conversation()?,
                agent: coordinate.agent()?,
                kind,
                text: String::new(),
                omitted_chars: 0,
                created_at: RuntimeEventTime::from_unix_millis(observed_at),
                event_id: None,
            });
            append_capped_text(pending, fragment);
            let rendered = if pending.omitted_chars == 0 {
                pending.text.clone()
            } else {
                format!(
                    "{}...<truncated {} chars>",
                    pending.text, pending.omitted_chars
                )
            };
            (
                RuntimeActivityFact::new(
                    pending.conversation.clone(),
                    pending.agent.clone(),
                    pending.kind,
                    RuntimeActivityPayload::Text { text: rendered },
                    pending.created_at,
                ),
                pending.event_id,
            )
        };
        let id = self
            .durable
            .upsert_text_activity(UpsertRuntimeTextActivity::new(
                admission.clone(),
                fact.clone(),
                previous_event_id,
                RuntimeEventTime::from_unix_millis(presence_now()),
            ))
            .await?
            .ok_or(DaemonRuntimeEventError::StaleRuntime)?;
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| DaemonRuntimeEventError::StatePoisoned)?;
            if let Some(pending) = state
                .runtimes
                .get_mut(&admission)
                .and_then(|entry| entry.pending_text.as_mut())
                && pending.kind == kind
            {
                pending.event_id = Some(id.id());
            }
        }
        let presence = self.publish_presence(id.presence_viewers(), fact.agent());
        let agent_projection = self.publish_conversation(agent_event);
        let activity = self.publish_conversation(ConversationRuntimeProjectionFact::Activity {
            scope: admission.principal().scope(),
            id: id.id(),
            fact,
        });
        finish_live_effects([presence, agent_projection, activity]).map_err(Into::into)
    }

    async fn record_and_publish(
        &self,
        admission: RuntimeEventAdmission,
        fact: RuntimeActivityFact,
        agent_event: Option<ConversationRuntimeProjectionFact>,
    ) -> Result<(), DaemonRuntimeEventError> {
        let id = self
            .durable
            .record_activity(
                admission.clone(),
                fact.clone(),
                RuntimeEventTime::from_unix_millis(presence_now()),
            )
            .await?
            .ok_or(DaemonRuntimeEventError::StaleRuntime)?;
        let presence = self.publish_presence(id.presence_viewers(), fact.agent());
        let agent_projection = agent_event
            .map(|event| self.publish_conversation(event))
            .unwrap_or(Ok(()));
        let activity = self.publish_conversation(ConversationRuntimeProjectionFact::Activity {
            scope: admission.principal().scope(),
            id: id.id(),
            fact,
        });
        finish_live_effects([presence, agent_projection, activity]).map_err(Into::into)
    }

    async fn current_admission(
        &self,
        host_id: &str,
        daemon: &DaemonInstanceId,
        scope: RuntimeScope,
    ) -> Result<RuntimeEventAdmission, DaemonRuntimeEventError> {
        let admission = self
            .durable
            .current_admission(host_id.to_owned(), scope)
            .await?
            .ok_or(DaemonRuntimeEventError::StaleRuntime)?;
        if admission.principal().custody().daemon_instance() != daemon {
            return Err(DaemonRuntimeEventError::StaleRuntime);
        }
        Ok(admission)
    }

    async fn admission_for_generation(
        &self,
        host_id: &str,
        daemon: &DaemonInstanceId,
        coordinate: &RuntimeCoordinate,
        generation: i64,
    ) -> Result<RuntimeEventAdmission, DaemonRuntimeEventError> {
        let principal = RuntimePrincipal::for_scope(
            coordinate.scope()?,
            RuntimeCustody::new(
                RuntimeGeneration::new(generation).map_err(DaemonRuntimeEventError::source)?,
                daemon.clone(),
            ),
        );
        let current = self
            .durable
            .current_admission(host_id.to_owned(), principal.scope())
            .await?;
        if current.as_ref().map(RuntimeEventAdmission::principal) != Some(&principal) {
            return Err(DaemonRuntimeEventError::StaleRuntime);
        }
        Ok(current.expect("current admission was compared as present"))
    }

    fn publish_status(
        &self,
        targets: &[RuntimeStatusTarget],
        agent: &AgentId,
        update: RuntimeStatusProjectionUpdate,
    ) -> Result<(), LiveProjectionFailure> {
        self.status_projection.publish(RuntimeStatusProjectionFact {
            targets: targets.to_vec(),
            agent: agent.clone(),
            update,
        })
    }

    fn publish_conversation(
        &self,
        fact: ConversationRuntimeProjectionFact,
    ) -> Result<(), LiveProjectionFailure> {
        self.conversation_projection.publish(fact)
    }

    fn publish_presence(
        &self,
        viewers: &[silverhand_backend_core::foundation::identity::UserId],
        agent: &AgentId,
    ) -> Result<(), LiveProjectionFailure> {
        self.presence_effect.publish(viewers, agent)
    }
}

fn deserialize_optional_non_null<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn finish_live_effects<E, const N: usize>(effects: [Result<(), E>; N]) -> Result<(), E> {
    for effect in effects {
        effect?;
    }
    Ok(())
}

fn presence_now() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        Err(error) => -i64::try_from(error.duration().as_millis()).unwrap_or(i64::MAX),
    }
}

fn runtime_exit_status(
    init_failed: bool,
    fatal: bool,
    intentional: bool,
    code: Option<i64>,
) -> RuntimeObservedStatus {
    if init_failed || fatal || (!intentional && code != Some(0)) {
        RuntimeObservedStatus::Crashed
    } else {
        RuntimeObservedStatus::Idle
    }
}

fn runtime_exit_should_respawn(init_failed: bool, fatal: bool, intentional: bool) -> bool {
    !intentional && !init_failed && !fatal
}

#[derive(Debug)]
pub(crate) enum DaemonRuntimeEventError {
    Persistence(PersistenceFailure),
    IngressCodec(Box<dyn Error + Send + Sync>),
    LiveProjection(LiveProjectionFailure),
    StatePoisoned,
    StaleRuntime,
    InvalidPayload,
}

impl DaemonRuntimeEventError {
    fn source(error: impl Error + Send + Sync + 'static) -> Self {
        Self::IngressCodec(Box::new(error))
    }
}

impl fmt::Display for DaemonRuntimeEventError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Persistence(_) => "daemon runtime event persistence failed",
            Self::LiveProjection(_) => "daemon runtime event live projection failed",
            Self::IngressCodec(_) | Self::InvalidPayload => "daemon runtime event decode failed",
            Self::StatePoisoned => "daemon runtime event state lock was poisoned",
            Self::StaleRuntime => "daemon runtime event custody is stale",
        })
    }
}

impl Error for DaemonRuntimeEventError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Persistence(source) => Some(source),
            Self::IngressCodec(source) => Some(source.as_ref()),
            Self::LiveProjection(source) => Some(source),
            Self::StatePoisoned | Self::StaleRuntime | Self::InvalidPayload => None,
        }
    }
}

impl From<PersistenceFailure> for DaemonRuntimeEventError {
    fn from(value: PersistenceFailure) -> Self {
        Self::Persistence(value)
    }
}
impl From<LiveProjectionFailure> for DaemonRuntimeEventError {
    fn from(value: LiveProjectionFailure) -> Self {
        Self::LiveProjection(value)
    }
}
impl From<serde_json::Error> for DaemonRuntimeEventError {
    fn from(value: serde_json::Error) -> Self {
        Self::IngressCodec(Box::new(value))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum RuntimeScopeWire {
    Agent {
        agent_id: String,
    },
    Conversation {
        conversation_id: String,
        agent_id: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeCoordinate {
    pub runtime_scope: RuntimeScopeWire,
    pub conversation_id: String,
    pub agent_id: String,
}

impl RuntimeCoordinate {
    fn scope(&self) -> Result<RuntimeScope, DaemonRuntimeEventError> {
        let agent = self.agent()?;
        match &self.runtime_scope {
            RuntimeScopeWire::Agent { agent_id } if agent_id == &self.agent_id => {
                Ok(RuntimeScope::AgentSlot { agent })
            }
            RuntimeScopeWire::Conversation {
                conversation_id,
                agent_id,
            } if conversation_id == &self.conversation_id && agent_id == &self.agent_id => {
                Ok(RuntimeScope::ConversationSlot {
                    agent,
                    source: self.conversation()?,
                })
            }
            _ => Err(DaemonRuntimeEventError::InvalidPayload),
        }
    }
    fn agent(&self) -> Result<AgentId, DaemonRuntimeEventError> {
        AgentId::parse(self.agent_id.clone()).map_err(DaemonRuntimeEventError::source)
    }
    fn conversation(&self) -> Result<ConversationId, DaemonRuntimeEventError> {
        ConversationId::parse(self.conversation_id.clone()).map_err(DaemonRuntimeEventError::source)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentRunStateWire {
    pub run_id: String,
    pub agent_id: String,
    pub conversation_id: String,
    pub status: AgentRunStatus,
    pub started_at: serde_json::Number,
    pub updated_at: serde_json::Number,
    pub last_activity_at: serde_json::Number,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub terminal_reason: Option<AgentRunTerminalReason>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub activity_snippet: Option<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub activity_snippet_kind: Option<AgentActivitySnippetKind>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub tool_call_counts: Option<std::collections::BTreeMap<String, serde_json::Number>>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub compacting: Option<bool>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub compacting_since: Option<serde_json::Number>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub compaction_stale: Option<bool>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConversationAgentRunStateSnapshot {
    pub source_agent: AgentId,
    pub source_conversation: ConversationId,
    pub state: AgentRunStateWire,
}

impl AgentRunStateWire {
    /// Re-anchor a shared runtime fact without sharing any source content.
    pub(crate) fn shared_projection(&self, target: &ConversationId) -> Self {
        let mut value = self.clone();
        value.conversation_id = target.as_str().to_owned();
        value.activity_snippet = None;
        value.activity_snippet_kind = None;
        value.detail = None;
        value
    }

    pub(crate) fn matches_coordinate(&self, agent: &str, conversation: &str) -> bool {
        self.agent_id == agent && self.conversation_id == conversation
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentRunStatus {
    Starting,
    Thinking,
    ToolRunning,
    Waiting,
    Idle,
    Failed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentRunTerminalReason {
    Replied,
    TurnEnded,
    Replaced,
    Stopped,
    Crashed,
    Timeout,
    ConversationClosed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentActivitySnippetKind {
    Thinking,
    Text,
    ApiResponse,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InitTimeoutReason {
    Silence,
    PreProtocolCap,
    PostProtocol,
}
impl InitTimeoutReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Silence => "silence",
            Self::PreProtocolCap => "pre_protocol_cap",
            Self::PostProtocol => "post_protocol",
        }
    }
}

macro_rules! coordinate_event {
    ($name:ident { $($field:ident : $ty:ty),* $(,)? }) => {
        #[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
        #[serde(deny_unknown_fields)]
        pub(crate) struct $name {
            #[serde(flatten)] pub coordinate: RuntimeCoordinate,
            $(pub $field: $ty,)*
        }
    };
}

coordinate_event!(RuntimeStateEvent {
    runtime_generation: i64,
    state: AgentRunStateWire,
    ts: i64
});
coordinate_event!(RuntimeOutputEvent {
    stream: RuntimeOutputStream,
    chunk: String,
    ts: i64
});
coordinate_event!(RuntimeReadyEvent {
    runtime_generation: i64,
    ts: i64
});
coordinate_event!(RuntimeInitTimeoutEvent { runtime_generation: i64, timeout_reason: InitTimeoutReason, elapsed_ms: i64, timeout_ms: i64, retrying: bool, retry_attempt: i64, max_retries: i64, cooldown_until: Option<i64>, ts: i64 });
coordinate_event!(RuntimeInitRetryingEvent {
    runtime_generation: i64,
    retry_attempt: i64,
    max_retries: i64,
    delay_ms: i64,
    ts: i64
});
coordinate_event!(RuntimeInitCooldownEvent {
    runtime_generation: i64,
    cooldown_until: i64,
    attempts_exhausted: i64,
    ts: i64
});
coordinate_event!(RuntimeCompactionStaleEvent {
    elapsed_ms: i64,
    ts: i64
});
coordinate_event!(RuntimeOutputDroppedEvent {
    dropped_count: i64,
    bytes_dropped: i64,
    since_ts: i64
});
coordinate_event!(RuntimeOutputBoundaryEvent {});
coordinate_event!(RuntimeParsedEvent {
    runtime_generation: i64,
    parsed: ParsedEventWire,
    ts: i64
});
coordinate_event!(RuntimeParsedDroppedEvent {
    parsed_kind: String,
    encoded_bytes: i64,
    reason: String,
    ts: i64
});
coordinate_event!(RuntimeMessageStaleEvent {
    runtime_generation: i64,
    ts: i64
});

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ParsedEventWire {
    SessionInit {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    Thinking {
        text: String,
    },
    Text {
        text: String,
    },
    ToolCall {
        name: String,
        input: Value,
        #[serde(rename = "toolCallId")]
        tool_call_id: Option<String>,
    },
    ToolCallInput {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        input: Value,
    },
    ToolOutput {
        name: Option<String>,
        #[serde(rename = "toolCallId")]
        tool_call_id: Option<String>,
        #[serde(default)]
        outcome: ToolOutcome,
    },
    TurnEnd {
        #[serde(rename = "sessionId")]
        session_id: Option<String>,
    },
    CompactionStarted,
    CompactionFinished,
    InputLifecycle {
        #[serde(rename = "commandUuid")]
        command_uuid: String,
        state: InputLifecycleState,
    },
    ApiResponse {
        status: ApiResponseStatus,
        message: String,
    },
    Error {
        message: String,
        code: Option<String>,
        fatal: Option<bool>,
    },
    Unknown {
        raw: String,
    },
}

macro_rules! parsed_event_kind_map {
    ($($pattern:pat => $kind:literal),+ $(,)?) => {
        impl ParsedEventWire {
            pub(crate) const IMPLEMENTED_KINDS: &'static [&'static str] = &[$($kind),+];

            pub(crate) fn kind(&self) -> &'static str {
                match self {
                    $($pattern => $kind),+
                }
            }
        }
    };
}

parsed_event_kind_map!(
    Self::SessionInit { .. } => "session_init",
    Self::Thinking { .. } => "thinking",
    Self::Text { .. } => "text",
    Self::ToolCall { .. } => "tool_call",
    Self::ToolCallInput { .. } => "tool_call_input",
    Self::ToolOutput { .. } => "tool_output",
    Self::TurnEnd { .. } => "turn_end",
    Self::CompactionStarted => "compaction_started",
    Self::CompactionFinished => "compaction_finished",
    Self::InputLifecycle { .. } => "input_lifecycle",
    Self::ApiResponse { .. } => "api_response",
    Self::Error { .. } => "error",
    Self::Unknown { .. } => "unknown",
);

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolOutcome {
    Success,
    Failed,
    Cancelled,
    #[default]
    Completed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InputLifecycleState {
    Queued,
    Started,
    Completed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ApiResponseStatus {
    Waiting,
    Ready,
    Notice,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimeOutputStream {
    Stdout,
    Stderr,
}

impl std::fmt::Display for RuntimeOutputStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        })
    }
}

impl ParsedEventWire {
    fn sanitized(&self) -> Self {
        match self {
            Self::ToolCall {
                name,
                input,
                tool_call_id,
            } => Self::ToolCall {
                name: name.clone(),
                input: sanitize_json_value(input, 0),
                tool_call_id: tool_call_id.clone(),
            },
            Self::ToolCallInput {
                tool_call_id,
                input,
            } => Self::ToolCallInput {
                tool_call_id: tool_call_id.clone(),
                input: sanitize_json_value(input, 0),
            },
            other => other.clone(),
        }
    }

    fn activity(
        &self,
        coordinate: &RuntimeCoordinate,
        ts: i64,
    ) -> Result<Option<RuntimeActivityFact>, DaemonRuntimeEventError> {
        let (kind, payload) = match self {
            Self::SessionInit { session_id } => (
                RuntimeActivityKind::SessionInit,
                RuntimeActivityPayload::Text {
                    text: session_id.clone(),
                },
            ),
            Self::Thinking { text } => (
                RuntimeActivityKind::Thinking,
                RuntimeActivityPayload::Text { text: text.clone() },
            ),
            Self::Text { text } => (
                RuntimeActivityKind::Text,
                RuntimeActivityPayload::Text { text: text.clone() },
            ),
            Self::ToolCall {
                name,
                input,
                tool_call_id,
            } => (
                RuntimeActivityKind::ToolCall,
                RuntimeActivityPayload::ToolCall {
                    name: name.clone(),
                    input_json: serde_json::to_string(&sanitize_json_value(input, 0))?,
                    tool_call_id: tool_call_id.clone(),
                },
            ),
            Self::ToolCallInput { .. } | Self::InputLifecycle { .. } | Self::Unknown { .. } => {
                return Ok(None);
            }
            Self::ToolOutput {
                name,
                tool_call_id,
                outcome,
            } => (
                RuntimeActivityKind::ToolOutput,
                RuntimeActivityPayload::ToolOutput {
                    name: name.clone(),
                    tool_call_id: tool_call_id.clone(),
                    outcome: serde_json::to_value(outcome)?
                        .as_str()
                        .unwrap_or("completed")
                        .to_owned(),
                },
            ),
            Self::TurnEnd { .. } => (RuntimeActivityKind::TurnEnd, RuntimeActivityPayload::None),
            Self::CompactionStarted => (
                RuntimeActivityKind::CompactionStarted,
                RuntimeActivityPayload::None,
            ),
            Self::CompactionFinished => (
                RuntimeActivityKind::CompactionFinished,
                RuntimeActivityPayload::None,
            ),
            Self::ApiResponse { status, message } => (
                RuntimeActivityKind::ApiResponse,
                RuntimeActivityPayload::ApiResponse {
                    status: serde_json::to_value(status)?
                        .as_str()
                        .unwrap_or("notice")
                        .to_owned(),
                    message: message.clone(),
                },
            ),
            Self::Error {
                message,
                code,
                fatal,
            } => (
                RuntimeActivityKind::Error,
                RuntimeActivityPayload::Error {
                    message: message.clone(),
                    code: code.clone(),
                    fatal: *fatal,
                },
            ),
        };
        Ok(Some(RuntimeActivityFact::new(
            coordinate.conversation()?,
            coordinate.agent()?,
            kind,
            payload,
            RuntimeEventTime::from_unix_millis(ts),
        )))
    }
}

fn append_capped_text(pending: &mut PendingTextActivity, fragment: &str) {
    let used = pending.text.chars().count();
    let available = TEXT_ACTIVITY_MAX_CHARS.saturating_sub(used);
    if available == 0 {
        pending.omitted_chars = pending
            .omitted_chars
            .saturating_add(fragment.chars().count());
        return;
    }
    let kept = fragment.chars().take(available).collect::<String>();
    let kept_count = kept.chars().count();
    pending.text.push_str(&kept);
    pending.omitted_chars = pending
        .omitted_chars
        .saturating_add(fragment.chars().count().saturating_sub(kept_count));
}

fn sanitize_json_value(value: &Value, depth: usize) -> Value {
    const STRING_MAX: usize = 500;
    const ARRAY_MAX: usize = 20;
    const OBJECT_MAX: usize = 40;
    const DEPTH_MAX: usize = 4;

    match value {
        Value::String(text) => {
            let length = text.encode_utf16().count();
            if length <= STRING_MAX {
                return value.clone();
            }
            let mut units = 0;
            let mut boundary = 0;
            for (index, character) in text.char_indices() {
                let next = units + character.len_utf16();
                if next > STRING_MAX {
                    break;
                }
                units = next;
                boundary = index + character.len_utf8();
            }
            Value::String(format!(
                "{}...<truncated {} chars>",
                &text[..boundary],
                length.saturating_sub(units)
            ))
        }
        Value::Array(items) if depth >= DEPTH_MAX => Value::String("[Truncated object]".to_owned()),
        Value::Array(items) => {
            let mut output = items
                .iter()
                .take(ARRAY_MAX)
                .map(|item| sanitize_json_value(item, depth + 1))
                .collect::<Vec<_>>();
            if items.len() > output.len() {
                output.push(json!({ "__truncatedItems": items.len() - output.len() }));
            }
            Value::Array(output)
        }
        Value::Object(object) if depth >= DEPTH_MAX => {
            Value::String("[Truncated object]".to_owned())
        }
        Value::Object(object) => {
            let mut output = serde_json::Map::new();
            for (key, value) in object.iter().take(OBJECT_MAX) {
                output.insert(key.clone(), sanitize_json_value(value, depth + 1));
            }
            if object.len() > output.len() {
                output.insert(
                    "__truncatedKeys".to_owned(),
                    Value::from(object.len() - output.len()),
                );
            }
            Value::Object(output)
        }
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Number;
    use silverhand_backend_core::{
        foundation::identity::{AgentId, ConversationId, DaemonInstanceId},
        host_runtime::runtime_credential::{
            RuntimeCustody, RuntimeGeneration, RuntimePrincipal, RuntimeScope,
        },
        host_runtime::runtime_event::{RuntimeEventAdmission, RuntimeObservedStatus},
    };

    use super::{
        AgentRunStateWire, AgentRunStatus, DaemonRuntimeEventError, RuntimeEventSettlement,
        RuntimeOwnerState, runtime_exit_should_respawn, runtime_exit_status,
    };

    fn admission(
        host: &str,
        agent: &str,
        conversation: &str,
        generation: i64,
    ) -> RuntimeEventAdmission {
        RuntimeEventAdmission::new(
            host.to_owned(),
            RuntimePrincipal::for_scope(
                RuntimeScope::ConversationSlot {
                    agent: AgentId::parse(agent).unwrap(),
                    source: ConversationId::parse(conversation).unwrap(),
                },
                RuntimeCustody::new(
                    RuntimeGeneration::new(generation).unwrap(),
                    DaemonInstanceId::parse(format!("daemon-{host}")).unwrap(),
                ),
            ),
        )
    }

    fn run_state(agent: &str, conversation: &str, run: &str) -> AgentRunStateWire {
        AgentRunStateWire {
            run_id: run.to_owned(),
            agent_id: agent.to_owned(),
            conversation_id: conversation.to_owned(),
            status: AgentRunStatus::Thinking,
            started_at: Number::from(1),
            updated_at: Number::from(2),
            last_activity_at: Number::from(2),
            terminal_reason: None,
            activity_snippet: None,
            activity_snippet_kind: None,
            tool_call_counts: None,
            compacting: None,
            compacting_since: None,
            compaction_stale: None,
            detail: None,
        }
    }

    fn owner(
        entries: impl IntoIterator<Item = (RuntimeEventAdmission, AgentRunStateWire)>,
    ) -> RuntimeEventSettlement<()> {
        let hub = crate::foundation::scope_event_transport::ScopeEventHub::production().unwrap();
        let owner = RuntimeEventSettlement {
            durable: (),
            conversation_projection: std::sync::Arc::new(hub.runtime_activity_publisher()),
            presence_effect: std::sync::Arc::new(hub.agent_presence_publisher()),
            status_projection: std::sync::Arc::new(hub.runtime_status_publisher()),
            state: std::sync::Arc::new(std::sync::Mutex::new(RuntimeOwnerState::default())),
        };
        {
            let mut state = owner.state.lock().unwrap();
            for (admission, run) in entries {
                state.runtimes.entry(admission).or_default().run = Some(run);
            }
        }
        owner
    }

    #[test]
    fn init_and_fatal_failures_close_the_respawn_gate_without_faking_idle() {
        for (init_failed, fatal) in [(true, false), (false, true)] {
            assert_eq!(
                runtime_exit_status(init_failed, fatal, false, Some(1)),
                RuntimeObservedStatus::Crashed
            );
            assert!(!runtime_exit_should_respawn(init_failed, fatal, false));
        }

        assert_eq!(
            runtime_exit_status(false, false, false, Some(1)),
            RuntimeObservedStatus::Crashed
        );
        assert!(runtime_exit_should_respawn(false, false, false));
        assert_eq!(
            runtime_exit_status(false, false, true, Some(0)),
            RuntimeObservedStatus::Idle
        );
        assert!(!runtime_exit_should_respawn(false, false, true));
    }

    #[test]
    fn owner_snapshot_filters_honest_foreign_entries_and_orders_one_atomic_map_copy() {
        let owner = owner([
            (
                admission("host-b", "agent-b", "c", 1),
                run_state("agent-b", "c", "run-b"),
            ),
            (
                admission("host-a", "agent-a", "other", 1),
                run_state("agent-a", "other", "run-other"),
            ),
            (
                admission("host-a", "agent-a", "c", 2),
                run_state("agent-a", "c", "run-a"),
            ),
        ]);

        let snapshot = owner
            .candidate_run_states(&ConversationId::parse("c").unwrap())
            .unwrap();
        assert_eq!(
            snapshot
                .iter()
                .map(|(_, snapshot)| {
                    (
                        snapshot.state.agent_id.as_str(),
                        snapshot.state.run_id.as_str(),
                    )
                })
                .collect::<Vec<_>>(),
            [("agent-a", "run-a"), ("agent-b", "run-b")]
        );
    }

    #[test]
    fn owner_snapshot_rejects_both_outer_inner_conversation_splice_directions() {
        for (outer_conversation, inner_conversation) in [("foreign", "c"), ("c", "foreign")] {
            let owner = owner([(
                admission("host-a", "agent-a", outer_conversation, 1),
                run_state("agent-a", inner_conversation, "run-a"),
            )]);

            assert!(matches!(
                owner.candidate_run_states(&ConversationId::parse("c").unwrap()),
                Err(DaemonRuntimeEventError::InvalidPayload)
            ));
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HydratedRuntime {
    pub principal: RuntimePrincipal,
    pub run_state: Option<AgentRunStateWire>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeProductEvent {
    State(RuntimeStateEvent),
    Output(RuntimeOutputEvent),
    ReadyForDrain(RuntimeReadyEvent),
    InitTimeout(RuntimeInitTimeoutEvent),
    InitRetrying(RuntimeInitRetryingEvent),
    InitCooldown(RuntimeInitCooldownEvent),
    CompactionStale(RuntimeCompactionStaleEvent),
    OutputDropped(RuntimeOutputDroppedEvent),
    OutputPaused(RuntimeOutputBoundaryEvent),
    OutputResumed(RuntimeOutputBoundaryEvent),
    Parsed(RuntimeParsedEvent),
    ParsedDropped(RuntimeParsedDroppedEvent),
    MessageStale(RuntimeMessageStaleEvent),
}

pub(crate) fn decode_product_event(
    kind: &str,
    value: &Value,
) -> Result<RuntimeProductEvent, serde_json::Error> {
    fn body<T: DeserializeOwned>(value: &Value) -> Result<T, serde_json::Error> {
        let mut body = value.as_object().cloned().unwrap_or_default();
        body.remove("direction");
        body.remove("type");
        body.remove("kind");
        serde_json::from_value(Value::Object(body))
    }
    let expected: &[&[&str]] = match kind {
        "runtime_state" => &[&[
            "direction",
            "type",
            "kind",
            "runtime_scope",
            "conversation_id",
            "agent_id",
            "runtime_generation",
            "state",
            "ts",
        ]],
        "runtime_output" => &[&[
            "direction",
            "type",
            "kind",
            "runtime_scope",
            "conversation_id",
            "agent_id",
            "stream",
            "chunk",
            "ts",
        ]],
        "runtime_ready_for_drain" => &[&[
            "direction",
            "type",
            "kind",
            "runtime_scope",
            "conversation_id",
            "agent_id",
            "runtime_generation",
            "ts",
        ]],
        "runtime_init_timeout" => &[
            &[
                "direction",
                "type",
                "kind",
                "runtime_scope",
                "conversation_id",
                "agent_id",
                "runtime_generation",
                "timeout_reason",
                "elapsed_ms",
                "timeout_ms",
                "retrying",
                "retry_attempt",
                "max_retries",
                "ts",
            ],
            &[
                "direction",
                "type",
                "kind",
                "runtime_scope",
                "conversation_id",
                "agent_id",
                "runtime_generation",
                "timeout_reason",
                "elapsed_ms",
                "timeout_ms",
                "retrying",
                "retry_attempt",
                "max_retries",
                "cooldown_until",
                "ts",
            ],
        ],
        "runtime_init_retrying" => &[&[
            "direction",
            "type",
            "kind",
            "runtime_scope",
            "conversation_id",
            "agent_id",
            "runtime_generation",
            "retry_attempt",
            "max_retries",
            "delay_ms",
            "ts",
        ]],
        "runtime_init_cooldown" => &[&[
            "direction",
            "type",
            "kind",
            "runtime_scope",
            "conversation_id",
            "agent_id",
            "runtime_generation",
            "cooldown_until",
            "attempts_exhausted",
            "ts",
        ]],
        "runtime_compaction_stale" => &[&[
            "direction",
            "type",
            "kind",
            "runtime_scope",
            "conversation_id",
            "agent_id",
            "elapsed_ms",
            "ts",
        ]],
        "runtime_output_dropped" => &[&[
            "direction",
            "type",
            "kind",
            "runtime_scope",
            "conversation_id",
            "agent_id",
            "dropped_count",
            "bytes_dropped",
            "since_ts",
        ]],
        "runtime_output_paused" | "runtime_output_resumed" => &[&[
            "direction",
            "type",
            "kind",
            "runtime_scope",
            "conversation_id",
            "agent_id",
        ]],
        "runtime_parsed_event" => &[&[
            "direction",
            "type",
            "kind",
            "runtime_scope",
            "conversation_id",
            "agent_id",
            "runtime_generation",
            "parsed",
            "ts",
        ]],
        "runtime_parsed_event_dropped" => &[&[
            "direction",
            "type",
            "kind",
            "runtime_scope",
            "conversation_id",
            "agent_id",
            "parsed_kind",
            "encoded_bytes",
            "reason",
            "ts",
        ]],
        "runtime_message_stale" => &[&[
            "direction",
            "type",
            "kind",
            "runtime_scope",
            "conversation_id",
            "agent_id",
            "runtime_generation",
            "ts",
        ]],
        _ => {
            return Err(serde_json::Error::custom(
                "unsupported daemon product event kind",
            ));
        }
    };
    let object = value
        .as_object()
        .ok_or_else(|| serde_json::Error::custom("daemon event must be an object"))?;
    if !expected
        .iter()
        .any(|keys| object.len() == keys.len() && keys.iter().all(|key| object.contains_key(*key)))
    {
        return Err(serde_json::Error::custom(
            "daemon event keys do not match the published arm",
        ));
    }
    fn reject_null(
        object: &serde_json::Map<String, Value>,
        field: &str,
    ) -> Result<(), serde_json::Error> {
        if object.get(field).is_some_and(Value::is_null) {
            Err(serde_json::Error::custom(format!(
                "{field} must be omitted rather than null"
            )))
        } else {
            Ok(())
        }
    }
    match kind {
        "runtime_init_timeout" => reject_null(object, "cooldown_until")?,
        "runtime_state" => {
            let state = object
                .get("state")
                .and_then(Value::as_object)
                .ok_or_else(|| serde_json::Error::custom("state must be an object"))?;
            for field in [
                "terminalReason",
                "activitySnippet",
                "activitySnippetKind",
                "toolCallCounts",
                "compacting",
                "compactingSince",
                "compactionStale",
                "detail",
            ] {
                reject_null(state, field)?;
            }
        }
        "runtime_parsed_event" => {
            let parsed = object
                .get("parsed")
                .and_then(Value::as_object)
                .ok_or_else(|| serde_json::Error::custom("parsed must be an object"))?;
            let parsed_kind = parsed
                .get("kind")
                .and_then(Value::as_str)
                .ok_or_else(|| serde_json::Error::custom("parsed kind must be a string"))?;
            if !ParsedEventWire::IMPLEMENTED_KINDS.contains(&parsed_kind) {
                return Err(serde_json::Error::custom(
                    "parsed kind is not implemented by the Rust owner",
                ));
            }
            match parsed.get("kind").and_then(Value::as_str) {
                Some("tool_call") => reject_null(parsed, "toolCallId")?,
                Some("tool_output") => {
                    reject_null(parsed, "name")?;
                    reject_null(parsed, "toolCallId")?;
                }
                Some("turn_end") => reject_null(parsed, "sessionId")?,
                Some("error") => {
                    reject_null(parsed, "code")?;
                    reject_null(parsed, "fatal")?;
                }
                _ => {}
            }
        }
        _ => {}
    }
    let event = match kind {
        "runtime_state" => body(value).map(RuntimeProductEvent::State),
        "runtime_output" => body(value).map(RuntimeProductEvent::Output),
        "runtime_ready_for_drain" => body(value).map(RuntimeProductEvent::ReadyForDrain),
        "runtime_init_timeout" => body(value).map(RuntimeProductEvent::InitTimeout),
        "runtime_init_retrying" => body(value).map(RuntimeProductEvent::InitRetrying),
        "runtime_init_cooldown" => body(value).map(RuntimeProductEvent::InitCooldown),
        "runtime_compaction_stale" => body(value).map(RuntimeProductEvent::CompactionStale),
        "runtime_output_dropped" => body(value).map(RuntimeProductEvent::OutputDropped),
        "runtime_output_paused" => body(value).map(RuntimeProductEvent::OutputPaused),
        "runtime_output_resumed" => body(value).map(RuntimeProductEvent::OutputResumed),
        "runtime_parsed_event" => body(value).map(RuntimeProductEvent::Parsed),
        "runtime_parsed_event_dropped" => body(value).map(RuntimeProductEvent::ParsedDropped),
        "runtime_message_stale" => body(value).map(RuntimeProductEvent::MessageStale),
        _ => unreachable!("kind was checked above"),
    }?;
    event.validate()?;
    Ok(event)
}

impl RuntimeProductEvent {
    fn validate(&self) -> Result<(), serde_json::Error> {
        fn positive(value: i64, field: &str) -> Result<(), serde_json::Error> {
            if value > 0 {
                Ok(())
            } else {
                Err(serde_json::Error::custom(format!(
                    "{field} must be positive"
                )))
            }
        }
        fn nonnegative(value: i64, field: &str) -> Result<(), serde_json::Error> {
            if value >= 0 {
                Ok(())
            } else {
                Err(serde_json::Error::custom(format!(
                    "{field} must be nonnegative"
                )))
            }
        }
        fn nonempty(value: &str, field: &str) -> Result<(), serde_json::Error> {
            if value.is_empty() {
                Err(serde_json::Error::custom(format!(
                    "{field} must be non-empty"
                )))
            } else {
                Ok(())
            }
        }
        let coordinate = match self {
            Self::State(event) => {
                positive(event.runtime_generation, "runtime_generation")?;
                &event.coordinate
            }
            Self::Output(event) => &event.coordinate,
            Self::ReadyForDrain(event) => {
                positive(event.runtime_generation, "runtime_generation")?;
                &event.coordinate
            }
            Self::InitTimeout(event) => {
                positive(event.runtime_generation, "runtime_generation")?;
                nonnegative(event.elapsed_ms, "elapsed_ms")?;
                nonnegative(event.timeout_ms, "timeout_ms")?;
                nonnegative(event.retry_attempt, "retry_attempt")?;
                nonnegative(event.max_retries, "max_retries")?;
                if let Some(value) = event.cooldown_until {
                    positive(value, "cooldown_until")?;
                }
                &event.coordinate
            }
            Self::InitRetrying(event) => {
                positive(event.runtime_generation, "runtime_generation")?;
                positive(event.retry_attempt, "retry_attempt")?;
                nonnegative(event.max_retries, "max_retries")?;
                nonnegative(event.delay_ms, "delay_ms")?;
                &event.coordinate
            }
            Self::InitCooldown(event) => {
                positive(event.runtime_generation, "runtime_generation")?;
                positive(event.cooldown_until, "cooldown_until")?;
                nonnegative(event.attempts_exhausted, "attempts_exhausted")?;
                &event.coordinate
            }
            Self::CompactionStale(event) => {
                nonnegative(event.elapsed_ms, "elapsed_ms")?;
                &event.coordinate
            }
            Self::OutputDropped(event) => {
                nonnegative(event.dropped_count, "dropped_count")?;
                nonnegative(event.bytes_dropped, "bytes_dropped")?;
                &event.coordinate
            }
            Self::OutputPaused(event) | Self::OutputResumed(event) => &event.coordinate,
            Self::Parsed(event) => {
                positive(event.runtime_generation, "runtime_generation")?;
                &event.coordinate
            }
            Self::ParsedDropped(event) => {
                nonnegative(event.encoded_bytes, "encoded_bytes")?;
                if event.reason != "frame_too_large" {
                    return Err(serde_json::Error::custom(
                        "invalid parsed-event drop reason",
                    ));
                }
                if !crate::application_orchestration::daemon_delivery::PARSED_EVENT_KINDS
                    .contains(&event.parsed_kind.as_str())
                {
                    return Err(serde_json::Error::custom("invalid parsed-event kind"));
                }
                &event.coordinate
            }
            Self::MessageStale(event) => {
                positive(event.runtime_generation, "runtime_generation")?;
                &event.coordinate
            }
        };
        nonempty(&coordinate.conversation_id, "conversation_id")?;
        nonempty(&coordinate.agent_id, "agent_id")?;
        match &coordinate.runtime_scope {
            RuntimeScopeWire::Agent { agent_id } => nonempty(agent_id, "runtime_scope.agent_id")?,
            RuntimeScopeWire::Conversation {
                conversation_id,
                agent_id,
            } => {
                nonempty(conversation_id, "runtime_scope.conversation_id")?;
                nonempty(agent_id, "runtime_scope.agent_id")?;
            }
        }
        if let Self::State(event) = self
            && !event.state.matches_coordinate(
                &event.coordinate.agent_id,
                &event.coordinate.conversation_id,
            )
        {
            return Err(serde_json::Error::custom(
                "runtime state coordinate does not match outer event",
            ));
        }
        if let Self::Parsed(event) = self {
            match &event.parsed {
                ParsedEventWire::SessionInit { session_id } => {
                    nonempty(session_id, "parsed.sessionId")?
                }
                ParsedEventWire::ToolCall {
                    name, tool_call_id, ..
                } => {
                    nonempty(name, "parsed.name")?;
                    if let Some(value) = tool_call_id {
                        nonempty(value, "parsed.toolCallId")?;
                    }
                }
                ParsedEventWire::ToolCallInput { tool_call_id, .. } => {
                    nonempty(tool_call_id, "parsed.toolCallId")?
                }
                ParsedEventWire::ToolOutput {
                    name, tool_call_id, ..
                } => {
                    if let Some(value) = name {
                        nonempty(value, "parsed.name")?;
                    }
                    if let Some(value) = tool_call_id {
                        nonempty(value, "parsed.toolCallId")?;
                    }
                }
                ParsedEventWire::TurnEnd { session_id } => {
                    if let Some(value) = session_id {
                        nonempty(value, "parsed.sessionId")?;
                    }
                }
                ParsedEventWire::InputLifecycle { command_uuid, .. } => {
                    nonempty(command_uuid, "parsed.commandUuid")?
                }
                ParsedEventWire::ApiResponse { message, .. } => {
                    nonempty(message, "parsed.message")?
                }
                ParsedEventWire::Thinking { .. }
                | ParsedEventWire::Text { .. }
                | ParsedEventWire::CompactionStarted
                | ParsedEventWire::CompactionFinished
                | ParsedEventWire::Error { .. }
                | ParsedEventWire::Unknown { .. } => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tool_output_marker_contract {
    use super::*;
    #[test]
    fn new_output_bodies_are_rejected_before_settlement() {
        let marker =
            serde_json::json!({"kind":"tool_output","toolCallId":"call-one","outcome":"success"});
        let parsed: ParsedEventWire = serde_json::from_value(marker.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap()["outcome"], "success");
        let mut body = marker;
        body["output"] = serde_json::json!("unrequested body");
        assert!(serde_json::from_value::<ParsedEventWire>(body).is_err());
    }
}
