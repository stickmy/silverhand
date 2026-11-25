//! Production-only adapters for the three reviewed migration leaves.
//!
//! This module is deliberately composition-owned: it binds consumer-shaped ports
//! to the process's one event hub, unread/attention authorities, participant reads,
//! and daemon runtime.  No permissive or no-op implementation exists here.

use std::{error::Error, fmt, time::Duration};

use serde_json::{Value, json};
use silverhand_backend_core::conversation_messaging::messaging::collaboration::RuntimeControlSeatPort;
use silverhand_backend_core::conversation_messaging::messaging::{
    AddressRefusal, ChannelAccess, DeliveryIntent, DirectPairAllocation, MessageId, TargetAddress,
    task::{
        ExistingSourceTask, SourceMessageId, TaskConversationBoardRead, TaskCreateAdmissionReads,
        TaskCreateConversationState, TaskCreateSourceRef, TaskParticipationReads,
    },
};
use silverhand_backend_core::foundation::persistence::PersistenceFailure;
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        collaboration::{
            activity::observe::{
                CommittedObservation, ObservationAftermath, ObservationEffectFailure,
                ObservedConversation,
            },
            attention_preference::{
                AttentionPreferenceAftermath, AttentionPreferenceEffectFailure,
                ChannelAttentionCommit, WorkspaceAttentionCommit,
            },
            attention_projection::AttentionProjectionPort,
            channel_directory::{
                BoundHumanChannelCreation, ChannelCreationPublicationFailure,
                HumanChannelCreateAftermath,
            },
            channel_lifecycle::{
                BoundChannelArchive, BoundChannelRename, BoundChannelVisibility,
                ChannelArchiveAftermath, ChannelMutationEffectFailure, ChannelRenameAftermath,
                ChannelVisibilityAftermath,
            },
            membership::{
                CommittedMembershipNotice, ConversationMembershipAftermath,
                ConversationMembershipChanged, MembershipRuntimeStop,
            },
            workspace_rename_messaging::ConversationAgentsReadPort,
        },
        post_commit::UnreadPublication,
        task::{BoardKey, TaskEventKind},
        unread::{UnreadAudience, UnreadReadModel},
    },
    foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId},
    host_runtime::{
        runtime_credential::RuntimeScope,
        scope_runtime_stop::{ScopeRuntimeStopPort, ScopeRuntimeStopTarget},
    },
    identity_access::access::{
        WorkspaceMembershipAftermath, WorkspaceMembershipChanged,
        WorkspaceViewerMembershipChangeKind, WorkspaceViewerMembershipChanged,
    },
};
use silverhand_backend_server::{
    conversation_messaging::messaging::{
        collaboration::conversation_log_debug::{
            ConversationLogDebugAdmissionFact, ConversationLogDebugAdmissionPort,
            ConversationLogDebugAdmissionRefusal,
        },
        task::{
            HumanConversationPost, ResolvedPlacement, ResolvedTaskTarget, TaskCreatePairAllocation,
            TaskCreateTargetIntent, TaskCreateTargetResolver, TaskTargetRefusal,
        },
    },
    conversation_messaging::{
        LiveConversationRuntimeDelivery, SealedConversationRuntimeDelivery,
        messaging::task::{TaskAftermathTrigger, TaskEffectExecute},
    },
    foundation::scope_event_transport::ScopeEventHub,
    scheduling::schedule_create::ports::{
        ResolvedScheduleTarget, ScheduleCapabilityDenied, ScheduleManageCapability,
        ScheduleTargetAllocation, ScheduleTargetIntent, ScheduleTargetRefusal,
        ScheduleTargetResolver,
    },
};
use silverhand_backend_sqlite::conversation_messaging::messaging::schedule::{
    CommittedScheduleWake, ProductionCronEvaluator, ScheduleWakePlacement, ScheduleWakeWriteError,
    SqliteScheduleStore, SqliteScheduleWakeWrite, SystemClock as ScheduleSystemClock,
};
use silverhand_backend_sqlite::conversation_messaging::messaging::{
    MessagingStore,
    collaboration::conversation_participation::{
        ConversationParticipationRefusal, SqliteConversationParticipationRead,
    },
};

#[derive(Debug)]
pub(super) struct LaneEffectError(String);

impl LaneEffectError {
    fn from_source(source: impl fmt::Display) -> Self {
        Self(source.to_string())
    }
}

impl fmt::Display for LaneEffectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for LaneEffectError {}

#[derive(Clone)]
pub(super) struct ProductionScheduleWake<E, U, D> {
    writer: SqliteScheduleWakeWrite<E>,
    hub: ScopeEventHub,
    unread: U,
    delivery: D,
}

impl<E, U, D> ProductionScheduleWake<E, U, D> {
    pub(super) fn new(
        writer: SqliteScheduleWakeWrite<E>,
        hub: ScopeEventHub,
        unread: U,
        delivery: D,
    ) -> Self {
        Self {
            writer,
            hub,
            unread,
            delivery,
        }
    }
}

impl<E, U, D> ProductionScheduleWake<E, U, D>
where
    U: UnreadReadModel + Clone + Send + Sync + 'static,
    D: SealedConversationRuntimeDelivery,
{
    async fn announce(&self, wake: &CommittedScheduleWake) -> Vec<String> {
        let mut failures = Vec::new();
        if let Err(error) = self
            .hub
            .publish_system_message(&wake.conversation, wake.message.clone())
        {
            failures.push(error.to_string());
        }
        match wake.placement {
            ScheduleWakePlacement::Workspace => {
                if let Err(error) = self
                    .hub
                    .unread_publisher(self.unread.clone())
                    .publish_unread(&wake.conversation)
                    .await
                {
                    failures.push(error.to_string());
                }
            }
            ScheduleWakePlacement::DirectPair => {
                match self.unread.unread_audience(&wake.conversation).await {
                    Ok(Some(UnreadAudience::DirectPair { viewers })) => {
                        for viewer in viewers {
                            if let Err(error) = self
                                .hub
                                .publish_direct_pairs_changed(&viewer, &wake.conversation)
                            {
                                failures.push(error.to_string());
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(error) => failures.push(error.to_string()),
                }
            }
        }
        if let Err(error) = self
            .delivery
            .deliver_to_sealed_recipient(&wake.conversation, &wake.owner, &wake.message_id)
            .await
        {
            failures.push(error.to_string());
        }
        failures
    }
}

impl<E, U, D> silverhand_backend_core::conversation_messaging::messaging::schedule::ScheduleWake
    for ProductionScheduleWake<E, U, D>
where
    E: silverhand_backend_core::conversation_messaging::messaging::environment::SendEntropy
        + silverhand_backend_core::conversation_messaging::messaging::environment::SendClock
        + Clone
        + Send
        + Sync
        + 'static,
    U: UnreadReadModel + Clone + Send + Sync + 'static,
    D: SealedConversationRuntimeDelivery,
{
    async fn deliver(
        &self,
        ctx: silverhand_backend_core::conversation_messaging::messaging::schedule::WakeContext<'_>,
    ) -> Result<(), silverhand_backend_core::conversation_messaging::messaging::schedule::WakeFailed>
    {
        use silverhand_backend_core::conversation_messaging::messaging::schedule::WakeFailed;
        let wake = match self.writer.write(ctx).await {
            Ok(wake) => wake,
            Err(ScheduleWakeWriteError::TargetUnavailable(reason)) => {
                match self.writer.write_dead_target_notice(ctx, &reason).await {
                    Ok(Some(notice)) => {
                        for failure in self.announce(&notice).await {
                            tracing::warn!(
                                failure = silverhand_backend_server::foundation::safe_log_field::safe_display(&failure),
                                "schedule dead-target owner notice effect failed"
                            );
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(
                            error =
                                silverhand_backend_server::foundation::safe_log_field::safe_display(
                                    &error
                                ),
                            "schedule dead-target owner notice write failed"
                        )
                    }
                }
                return Err(WakeFailed {
                    reason: reason.to_string(),
                });
            }
            Err(error) => {
                return Err(WakeFailed {
                    reason: error.to_string(),
                });
            }
        };
        let mut failures = self.announce(&wake).await.into_iter();
        if let Some(reason) = failures.next() {
            return Err(WakeFailed { reason });
        }
        Ok(())
    }
}

/// Owned schedule driver.  Its watch sender is also the create route's
/// committed-effect input: a create tightens the next sleep immediately, while
/// the sixty-second branch remains the durable missed-wake backstop.
#[derive(Clone)]
pub(super) struct ScheduleDriverHandle {
    rearm: tokio::sync::watch::Sender<u64>,
}

impl silverhand_backend_core::conversation_messaging::messaging::schedule::ScheduleCreateCommittedEffect
    for ScheduleDriverHandle
{
    async fn rearm_near_timer(&self) -> Result<(), PersistenceFailure> {
        self.rearm.send_modify(|generation| *generation = generation.wrapping_add(1));
        Ok(())
    }
}

impl ScheduleDriverHandle {
    pub(super) async fn start<E, U, D>(
        store: SqliteScheduleStore<ScheduleSystemClock>,
        wake: ProductionScheduleWake<E, U, D>,
    ) -> Result<Self, PersistenceFailure>
    where
        E: silverhand_backend_core::conversation_messaging::messaging::environment::SendEntropy
            + silverhand_backend_core::conversation_messaging::messaging::environment::SendClock
            + Clone
            + Send
            + Sync
            + 'static,
        U: UnreadReadModel + Clone + Send + Sync + 'static,
        D: SealedConversationRuntimeDelivery,
    {
        use silverhand_backend_core::conversation_messaging::messaging::schedule::{
            Clock, process_one,
        };
        let recovered = store.reset_stale_firing().await?;
        if recovered > 0 {
            tracing::warn!(count = recovered, "reset stale schedule fires at startup");
        }
        let (sender, mut receiver) = tokio::sync::watch::channel(0_u64);
        tokio::spawn(async move {
            loop {
                let now = ScheduleSystemClock.now();
                match store.due_candidates(now, 200).await {
                    Ok(candidates) => {
                        for candidate in candidates {
                            if let Err(error) = process_one(
                                &store,
                                &wake,
                                &ScheduleSystemClock,
                                &ProductionCronEvaluator,
                                &candidate,
                            )
                            .await
                            {
                                tracing::error!(
                                    error = silverhand_backend_server::foundation::safe_log_field::safe_display(&error),
                                    schedule_id = candidate.id.as_str(),
                                    "schedule fire failed before settlement"
                                );
                            }
                        }
                    }
                    Err(error) => tracing::error!(
                        error = silverhand_backend_server::foundation::safe_log_field::safe_display(
                            &error
                        ),
                        "schedule due-row sweep failed"
                    ),
                }
                let now = ScheduleSystemClock.now();
                let delay = match store.next_pending_fire_at().await {
                    Ok(Some(next)) => {
                        Duration::from_millis(next.saturating_sub(now).clamp(0, 60_000) as u64)
                    }
                    Ok(None) => Duration::from_secs(60),
                    Err(error) => {
                        tracing::error!(
                            error =
                                silverhand_backend_server::foundation::safe_log_field::safe_display(
                                    &error
                                ),
                            "schedule next-fire read failed"
                        );
                        Duration::from_secs(60)
                    }
                };
                tokio::select! {
                    () = tokio::time::sleep(delay) => {},
                    changed = receiver.changed() => {
                        if changed.is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Ok(Self { rearm: sender })
    }
}

#[derive(Clone)]
pub(super) struct CanonicalTaskTarget<R, B> {
    resolver: R,
    boards: B,
}

impl<R, B> CanonicalTaskTarget<R, B> {
    pub(super) fn new(resolver: R, boards: B) -> Self {
        Self { resolver, boards }
    }
}

impl<R, B> TaskCreateTargetResolver for CanonicalTaskTarget<R, B>
where
    R: TargetAddress + Send + Sync,
    B: TaskConversationBoardRead + Clone + Send + Sync,
{
    type Error = LaneEffectError;

    async fn resolve(
        &self,
        agent: &AgentId,
        address: &str,
        allocation: TaskCreatePairAllocation,
        intent: TaskCreateTargetIntent,
    ) -> Result<Result<ResolvedTaskTarget, TaskTargetRefusal>, Self::Error> {
        let TaskCreatePairAllocation::Permitted = allocation;
        let TaskCreateTargetIntent::Write = intent;
        let resolved = self
            .resolver
            .resolve(
                agent,
                address,
                DirectPairAllocation::Permitted,
                DeliveryIntent::Write,
            )
            .await
            .map_err(LaneEffectError::from_source)?;
        let resolved = match resolved {
            Ok(value) => value,
            Err(refusal) => {
                return Ok(Err(match refusal {
                    AddressRefusal::Malformed => TaskTargetRefusal::Malformed,
                    AddressRefusal::SelfPair => TaskTargetRefusal::SelfPairNotAddressable,
                    AddressRefusal::Unresolved | AddressRefusal::DirectMessageScope => {
                        TaskTargetRefusal::Unresolved
                    }
                    AddressRefusal::ThreadNotInTarget => TaskTargetRefusal::ThreadNotInTarget,
                }));
            }
        };
        let board = self
            .boards
            .conversation_board(&resolved.conversation)
            .await
            .map_err(LaneEffectError::from_source)?
            .ok_or_else(|| LaneEffectError("resolved task target disappeared".into()))?;
        let placement = match board {
            BoardKey::Workspace(workspace) => ResolvedPlacement::Workspace { workspace },
            BoardKey::Pair(_) => ResolvedPlacement::DirectPair,
        };
        Ok(Ok(ResolvedTaskTarget {
            conversation: resolved.conversation,
            thread_root: resolved
                .thread_root
                .map(|root| {
                    silverhand_backend_core::conversation_messaging::messaging::task::ThreadRootId::parse(
                        root.as_str().to_owned(),
                    )
                    .expect("canonical target resolver never returns an empty thread root")
                }),
            placement,
        }))
    }
}

#[derive(Clone)]
pub(super) struct CanonicalScheduleTarget<R>(pub(super) R);

impl<R> ScheduleTargetResolver for CanonicalScheduleTarget<R>
where
    R: TargetAddress + Send + Sync,
{
    type Error = LaneEffectError;

    async fn resolve(
        &self,
        agent: &AgentId,
        address: &str,
        allocation: ScheduleTargetAllocation,
        intent: ScheduleTargetIntent,
    ) -> Result<Result<ResolvedScheduleTarget, ScheduleTargetRefusal>, Self::Error> {
        let ScheduleTargetAllocation::Permitted = allocation;
        let ScheduleTargetIntent::Write = intent;
        let resolved = self
            .0
            .resolve(
                agent,
                address,
                DirectPairAllocation::Permitted,
                DeliveryIntent::Write,
            )
            .await
            .map_err(LaneEffectError::from_source)?;
        Ok(match resolved {
            Ok(value) if value.thread_root.is_some() => {
                Err(ScheduleTargetRefusal::ThreadSuffixUnsupported)
            }
            Ok(value) => Ok(ResolvedScheduleTarget {
                conversation: value.conversation,
            }),
            Err(AddressRefusal::Malformed) => Err(ScheduleTargetRefusal::Malformed),
            Err(AddressRefusal::SelfPair) => Err(ScheduleTargetRefusal::SelfPairNotAddressable),
            Err(AddressRefusal::Unresolved | AddressRefusal::DirectMessageScope) => {
                Err(ScheduleTargetRefusal::Unresolved)
            }
            Err(AddressRefusal::ThreadNotInTarget) => Err(ScheduleTargetRefusal::ThreadNotInTarget),
        })
    }
}

#[derive(Clone)]
pub(super) struct ScheduleCapability(pub(super) MessagingStore);

impl ScheduleManageCapability for ScheduleCapability {
    type Error = LaneEffectError;

    async fn may_manage_schedules(
        &self,
        agent: &AgentId,
        destination: &ConversationId,
    ) -> Result<Result<(), ScheduleCapabilityDenied>, Self::Error> {
        self.0
            .may_manage_schedules(agent, destination)
            .await
            .map_err(LaneEffectError::from_source)
            .map(|granted| granted.then_some(()).ok_or(ScheduleCapabilityDenied))
    }
}

#[derive(Clone)]
pub(super) struct HumanPost(pub(super) MessagingStore);

impl HumanConversationPost for HumanPost {
    async fn can_post(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<bool, PersistenceFailure> {
        Ok(self.0.may_post(conversation, viewer).await?.is_ok())
    }
}

/// Shared-pool task-create admission/board adapter. Each trait method delegates
/// to the canonical `MessagingStore` implementation rather than copying SQL.
#[derive(Clone)]
pub(super) struct TaskBoards(pub(super) MessagingStore);

impl TaskConversationBoardRead for TaskBoards {
    async fn conversation_board(
        &self,
        conversation: &ConversationId,
    ) -> Result<Option<BoardKey>, PersistenceFailure> {
        use silverhand_backend_core::conversation_messaging::messaging::task::TaskWriteDoorReads;
        let mut store = self.0.clone();
        store.conversation_board(conversation).await
    }
}

impl TaskCreateAdmissionReads for TaskBoards {
    async fn agent_exists(&self, agent: &AgentId) -> Result<bool, PersistenceFailure> {
        self.0.agent_exists(agent).await
    }
    async fn conversation_state(
        &self,
        conversation: &ConversationId,
    ) -> Result<Option<TaskCreateConversationState>, PersistenceFailure> {
        self.0.conversation_state(conversation).await
    }
    async fn source_message(
        &self,
        source: &SourceMessageId,
    ) -> Result<Option<TaskCreateSourceRef>, PersistenceFailure> {
        self.0.source_message(source).await
    }
    async fn task_for_source(
        &self,
        source: &SourceMessageId,
    ) -> Result<Option<ExistingSourceTask>, PersistenceFailure> {
        self.0.task_for_source(source).await
    }
}

impl TaskParticipationReads for TaskBoards {
    async fn is_agent_participant(
        &mut self,
        conversation: &ConversationId,
        agent: &AgentId,
    ) -> Result<bool, PersistenceFailure> {
        self.0.is_agent_participant(conversation, agent).await
    }
    async fn is_user_participant(
        &mut self,
        conversation: &ConversationId,
        user: &UserId,
    ) -> Result<bool, PersistenceFailure> {
        self.0.is_user_participant(conversation, user).await
    }
    async fn agent_participating_conversations(
        &mut self,
        agent: &AgentId,
        conversations: &[ConversationId],
    ) -> Result<std::collections::HashSet<ConversationId>, PersistenceFailure> {
        self.0
            .agent_participating_conversations(agent, conversations)
            .await
    }
}

#[derive(Clone)]
pub(super) struct LogDebugAdmission(pub(super) SqliteConversationParticipationRead);

impl ConversationLogDebugAdmissionPort for LogDebugAdmission {
    type Error = LaneEffectError;

    async fn admit(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<
        Result<ConversationLogDebugAdmissionFact, ConversationLogDebugAdmissionRefusal>,
        Self::Error,
    > {
        match self
            .0
            .read(viewer, conversation)
            .await
            .map_err(LaneEffectError::from_source)?
        {
            Ok(_) => Ok(Ok(ConversationLogDebugAdmissionFact::selected(
                viewer.clone(),
                conversation.clone(),
            ))),
            Err(ConversationParticipationRefusal::NotFound) => {
                Ok(Err(ConversationLogDebugAdmissionRefusal::NotFound))
            }
            Err(ConversationParticipationRefusal::Forbidden) => {
                Ok(Err(ConversationLogDebugAdmissionRefusal::Forbidden))
            }
        }
    }
}

/// Resolves the current conversation-scoped seat immediately before dispatching
/// the daemon hard-stop. Missing/stale seats are already stopped; an exact live
/// `(host, scope, generation)` is the only value allowed onto the transport.
#[derive(Clone)]
pub(super) struct ProductionMembershipRuntimeStop<P, S> {
    seats: P,
    stop: S,
}

impl<P, S> ProductionMembershipRuntimeStop<P, S> {
    pub(super) fn new(seats: P, stop: S) -> Self {
        Self { seats, stop }
    }
}

impl<P, S> MembershipRuntimeStop for ProductionMembershipRuntimeStop<P, S>
where
    P: RuntimeControlSeatPort,
    S: ScopeRuntimeStopPort,
{
    type Error = LaneEffectError;

    async fn stop(
        &self,
        conversation: &ConversationId,
        agent: &AgentId,
    ) -> Result<(), Self::Error> {
        let scope = RuntimeScope::ConversationSlot {
            agent: agent.clone(),
            source: conversation.clone(),
        };
        let Some(snapshot) = self
            .seats
            .snapshot(&scope)
            .await
            .map_err(LaneEffectError::from_source)?
        else {
            return Ok(());
        };
        let bound = snapshot
            .bind(&scope)
            .map_err(LaneEffectError::from_source)?;
        let target = ScopeRuntimeStopTarget::new(
            bound.host().clone(),
            bound.scope().clone(),
            bound.generation(),
        );
        match self
            .stop
            .hard_stop(&target)
            .await
            .map_err(LaneEffectError::from_source)?
        {
            Ok(_) => Ok(()),
            Err(refusal) => Err(LaneEffectError(format!(
                "runtime stop refused: {refusal:?}"
            ))),
        }
    }
}

/// One real effect owner shared by the conversation-control, membership, and task
/// routers.  Its clones retain the same hub and shared-pool read authorities.
#[derive(Clone)]
pub(super) struct LaneEffects<U, A, G, D, S> {
    hub: ScopeEventHub,
    unread: U,
    attention: A,
    agents: G,
    delivery: D,
    runtime_stop: S,
}

impl<U, A, G, D, S> LaneEffects<U, A, G, D, S> {
    pub(super) fn new(
        hub: ScopeEventHub,
        unread: U,
        attention: A,
        agents: G,
        delivery: D,
        runtime_stop: S,
    ) -> Self {
        Self {
            hub,
            unread,
            attention,
            agents,
            delivery,
            runtime_stop,
        }
    }
}

impl<U, A, G, D, S> MembershipRuntimeStop for LaneEffects<U, A, G, D, S>
where
    U: Clone + Send + Sync + 'static,
    A: Clone + Send + Sync + 'static,
    G: Clone + Send + Sync + 'static,
    D: Clone + Send + Sync + 'static,
    S: MembershipRuntimeStop + Clone + Send + Sync + 'static,
{
    type Error = LaneEffectError;

    async fn stop(
        &self,
        conversation: &ConversationId,
        agent: &AgentId,
    ) -> Result<(), Self::Error> {
        self.runtime_stop
            .stop(conversation, agent)
            .await
            .map_err(LaneEffectError::from_source)
    }
}

impl<U, A, G, D, S> LaneEffects<U, A, G, D, S>
where
    U: UnreadReadModel + Clone + Send + Sync,
    A: AttentionProjectionPort + Clone + Send + Sync,
    G: Sync,
    D: Sync,
    S: Sync,
{
    async fn publish_badge(
        &self,
        viewer: &UserId,
        workspace: &WorkspaceId,
    ) -> Result<(), LaneEffectError> {
        let rollup = self
            .attention
            .badges(viewer)
            .await
            .map_err(LaneEffectError::from_source)?;
        let count = rollup
            .by_workspace()
            .iter()
            .find(|row| row.workspace() == workspace)
            .map_or(0, |row| row.count().get());
        self.hub
            .publish_attention_badge(viewer, workspace, count)
            .map_err(LaneEffectError::from_source)
    }

    async fn publish_unread(&self, conversation: &ConversationId) -> Result<(), LaneEffectError> {
        self.hub
            .unread_publisher(self.unread.clone())
            .publish_unread(conversation)
            .await
            .map_err(LaneEffectError::from_source)
    }
}

impl<U, A, G, D, S> ObservationAftermath for LaneEffects<U, A, G, D, S>
where
    U: UnreadReadModel + Clone + Send + Sync,
    A: AttentionProjectionPort + Clone + Send + Sync,
    G: Sync,
    D: Sync,
    S: Sync,
{
    async fn conversation_changed(
        &self,
        _fact: &CommittedObservation,
        conversation: &ObservedConversation,
    ) -> Result<(), ObservationEffectFailure> {
        self.publish_unread(conversation.conversation())
            .await
            .map_err(ObservationEffectFailure::from_source)
    }

    async fn badge_changed(
        &self,
        fact: &CommittedObservation,
        workspace: &WorkspaceId,
    ) -> Result<(), ObservationEffectFailure> {
        self.publish_badge(fact.viewer(), workspace)
            .await
            .map_err(ObservationEffectFailure::from_source)
    }

    async fn notifications_dismissed(
        &self,
        fact: &CommittedObservation,
    ) -> Result<(), ObservationEffectFailure> {
        self.hub
            .publish_notification_dismissals(
                fact.viewer(),
                fact.dismissed_notifications()
                    .iter()
                    .map(|id| id.as_str().to_owned())
                    .collect(),
            )
            .map_err(ObservationEffectFailure::from_source)
    }
}

impl<U, A, G, D, S> AttentionPreferenceAftermath for LaneEffects<U, A, G, D, S>
where
    U: UnreadReadModel + Clone + Send + Sync,
    A: AttentionProjectionPort + Clone + Send + Sync,
    G: Sync,
    D: Sync,
    S: Sync,
{
    async fn channel_preference_changed(
        &self,
        fact: &impl ChannelAttentionCommit,
    ) -> Result<(), AttentionPreferenceEffectFailure> {
        self.hub
            .publish_attention_preference_changed(
                fact.viewer(),
                fact.workspace(),
                Some(fact.conversation()),
            )
            .map_err(AttentionPreferenceEffectFailure::from_source)
    }

    async fn channel_badge_changed(
        &self,
        fact: &impl ChannelAttentionCommit,
    ) -> Result<(), AttentionPreferenceEffectFailure> {
        self.publish_badge(fact.viewer(), fact.workspace())
            .await
            .map_err(AttentionPreferenceEffectFailure::from_source)
    }

    async fn workspace_preference_changed(
        &self,
        fact: &impl WorkspaceAttentionCommit,
    ) -> Result<(), AttentionPreferenceEffectFailure> {
        self.hub
            .publish_attention_preference_changed(fact.viewer(), fact.workspace(), None)
            .map_err(AttentionPreferenceEffectFailure::from_source)
    }

    async fn workspace_badge_changed(
        &self,
        fact: &impl WorkspaceAttentionCommit,
    ) -> Result<(), AttentionPreferenceEffectFailure> {
        self.publish_badge(fact.viewer(), fact.workspace())
            .await
            .map_err(AttentionPreferenceEffectFailure::from_source)
    }
}

impl<U, A, G, D, S> HumanChannelCreateAftermath for LaneEffects<U, A, G, D, S>
where
    U: Sync,
    A: Sync,
    G: Sync,
    D: Sync,
    S: Sync,
{
    async fn workspace_channels_changed(
        &self,
        fact: &BoundHumanChannelCreation,
    ) -> Result<(), ChannelCreationPublicationFailure> {
        self.hub
            .publish_workspace_channels_changed(fact.workspace(), fact.channel().conversation())
            .map_err(ChannelCreationPublicationFailure::from_source)
    }

    fn title_address_taken(&self, fact: &BoundHumanChannelCreation) {
        tracing::warn!(
            workspace_id = fact.workspace().as_str(),
            conversation_id = fact.channel().conversation().as_str(),
            title_key = fact.address().key(),
            "channel title address committed"
        );
    }
}

impl<U, A, G, D, S> ChannelVisibilityAftermath for LaneEffects<U, A, G, D, S>
where
    U: Sync,
    A: Sync,
    G: Sync,
    D: Sync,
    S: Sync,
{
    async fn workspace_channels_changed(
        &self,
        fact: &BoundChannelVisibility,
    ) -> Result<(), ChannelMutationEffectFailure> {
        self.hub
            .publish_workspace_channels_changed(fact.workspace(), fact.row().conversation())
            .map_err(ChannelMutationEffectFailure::from_source)
    }
}

impl<U, A, G, D, S> ChannelArchiveAftermath for LaneEffects<U, A, G, D, S>
where
    U: UnreadReadModel + Clone + Send + Sync + 'static,
    A: AttentionProjectionPort + Clone + Send + Sync + 'static,
    G: ConversationAgentsReadPort + Clone + Send + Sync + 'static,
    D: Clone + Send + Sync + 'static,
    S: MembershipRuntimeStop + Clone + Send + Sync + 'static,
{
    fn start_runtime_cleanup(&self, fact: &BoundChannelArchive) {
        let conversation = fact.row().conversation().clone();
        let agents = self.agents.clone();
        let stop = self.runtime_stop.clone();
        tokio::spawn(async move {
            match agents.agent_ids(&conversation).await {
                Ok(recipients) => {
                    for agent in recipients {
                        if let Err(error) = stop.stop(&conversation, &agent).await {
                            tracing::warn!(
                                error = silverhand_backend_server::foundation::safe_log_field::safe_display(&error),
                                conversation_id = conversation.as_str(),
                                agent_id = agent.as_str(),
                                "archived-channel runtime stop failed"
                            );
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        error = silverhand_backend_server::foundation::safe_log_field::safe_display(
                            &error
                        ),
                        conversation_id = conversation.as_str(),
                        "archived-channel runtime audience read failed"
                    )
                }
            }
        });
    }

    async fn workspace_channels_changed(
        &self,
        fact: &BoundChannelArchive,
    ) -> Result<(), ChannelMutationEffectFailure> {
        self.hub
            .publish_workspace_channels_changed(fact.workspace(), fact.row().conversation())
            .map_err(ChannelMutationEffectFailure::from_source)
    }

    async fn unread_updated(
        &self,
        fact: &BoundChannelArchive,
    ) -> Result<(), ChannelMutationEffectFailure> {
        self.publish_unread(fact.row().conversation())
            .await
            .map_err(ChannelMutationEffectFailure::from_source)
    }
}

impl<U, A, G, D, S> ChannelRenameAftermath for LaneEffects<U, A, G, D, S>
where
    U: Sync,
    A: Sync,
    G: Sync,
    D: SealedConversationRuntimeDelivery,
    S: Sync,
{
    fn title_address_moved(&self, fact: &BoundChannelRename) {
        tracing::warn!(
            workspace_id = fact.workspace().as_str(),
            conversation_id = fact.row().conversation().as_str(),
            from_key = fact.old().map(|title| title.key()),
            to_key = fact.requested().key(),
            "channel title address moved"
        );
    }

    async fn deliver(
        &self,
        fact: &BoundChannelRename,
        recipient: &AgentId,
    ) -> Result<(), ChannelMutationEffectFailure> {
        self.delivery
            .deliver_to_sealed_recipient(
                fact.row().conversation(),
                recipient,
                fact.notice()
                    .expect("changed rename owns a notice")
                    .message(),
            )
            .await
            .map_err(ChannelMutationEffectFailure::from_source)
    }

    async fn publish_message(
        &self,
        fact: &BoundChannelRename,
    ) -> Result<(), ChannelMutationEffectFailure> {
        let notice = fact.notice().expect("changed rename owns a notice");
        self.hub
            .publish_system_message(
                notice.conversation(),
                json!({
                    "id": notice.message().as_str(),
                    "conversation_id": notice.conversation().as_str(),
                    "from_kind": "system",
                    "from_agent_id": Value::Null,
                    "from_user_id": Value::Null,
                    "author": Value::Null,
                    "content": notice.content(),
                    "metadata": {"subtype":"rename","old_name":notice.old().title(),"new_name":notice.new_title().title(),"scope":"conversation"},
                    "attachments": Value::Null,
                    "reactions": Value::Null,
                    "seq": notice.seq().get(),
                    "created_at": notice.created_at(),
                    "thread_root_id": Value::Null,
                    "linked_task": Value::Null,
                }),
            )
            .map_err(ChannelMutationEffectFailure::from_source)
    }

    async fn workspace_channels_changed(
        &self,
        fact: &BoundChannelRename,
    ) -> Result<(), ChannelMutationEffectFailure> {
        self.hub
            .publish_workspace_channels_changed(fact.workspace(), fact.row().conversation())
            .map_err(ChannelMutationEffectFailure::from_source)
    }
}

fn membership_message(notice: &CommittedMembershipNotice) -> Result<Value, serde_json::Error> {
    let metadata = serde_json::from_str::<serde_json::Map<String, Value>>(&notice.metadata)?;
    Ok(json!({
        "id": notice.message_id.as_str(),
        "conversation_id": notice.conversation_id.as_str(),
        "from_kind": "system",
        "from_agent_id": Value::Null,
        "from_user_id": Value::Null,
        "author": Value::Null,
        "content": notice.content,
        "metadata": Value::Object(metadata),
        "attachments": Value::Null,
        "reactions": Value::Null,
        "seq": notice.seq,
        "created_at": notice.created_at,
        "thread_root_id": Value::Null,
        "linked_task": Value::Null,
    }))
}

impl<U, A, G, D, S> LaneEffects<U, A, G, D, S>
where
    G: ConversationAgentsReadPort + Clone + Send + Sync,
    D: LiveConversationRuntimeDelivery,
{
    async fn announce_membership_notice(
        &self,
        notice: &CommittedMembershipNotice,
    ) -> Result<(), LaneEffectError> {
        let mut first = match membership_message(notice) {
            Ok(message) => self
                .hub
                .publish_system_message(&notice.conversation_id, message)
                .err()
                .map(LaneEffectError::from_source),
            Err(error) => Some(LaneEffectError::from_source(error)),
        };
        match self.agents.agent_ids(&notice.conversation_id).await {
            Ok(recipients) => {
                for agent in recipients {
                    if let Err(error) = self
                        .delivery
                        .deliver_to_live_recipient(
                            &notice.conversation_id,
                            &agent,
                            &notice.message_id,
                        )
                        .await
                        && first.is_none()
                    {
                        first = Some(LaneEffectError::from_source(error));
                    }
                }
            }
            Err(error) if first.is_none() => first = Some(LaneEffectError::from_source(error)),
            Err(_) => {}
        }
        if let Some(error) = first {
            return Err(error);
        }
        Ok(())
    }
}

impl<U, A, G, D, S> ConversationMembershipAftermath for LaneEffects<U, A, G, D, S>
where
    U: Clone + Send + Sync + 'static,
    A: Clone + Send + Sync + 'static,
    G: ConversationAgentsReadPort + Clone + Send + Sync + 'static,
    D: LiveConversationRuntimeDelivery,
    S: Clone + Send + Sync + 'static,
{
    type Error = LaneEffectError;

    async fn announce(&self, changed: &ConversationMembershipChanged) -> Result<(), Self::Error> {
        let mut first = None;
        if let Some(notice) = &changed.notice
            && let Err(error) = self.announce_membership_notice(notice).await
        {
            first = Some(error);
        }
        if let Err(error) = self
            .hub
            .publish_conversation_members_changed(&changed.conversation_id, &changed.workspace_id)
            && first.is_none()
        {
            first = Some(LaneEffectError::from_source(error));
        }
        if let Err(error) = self
            .hub
            .publish_workspace_channels_changed(&changed.workspace_id, &changed.conversation_id)
            && first.is_none()
        {
            first = Some(LaneEffectError::from_source(error));
        }
        if let Some(viewer) = &changed.workspace_enrolled_human
            && let Err(error) = self.hub.publish_viewer_workspace_membership_changed(
                viewer,
                &changed.workspace_id,
                "added",
                Some("member"),
            )
            && first.is_none()
        {
            first = Some(LaneEffectError::from_source(error));
        }
        first.map_or(Ok(()), Err)
    }
}

impl<U, A, G, D, S> WorkspaceMembershipAftermath for LaneEffects<U, A, G, D, S>
where
    U: Clone + Send + Sync + 'static,
    A: Clone + Send + Sync + 'static,
    G: ConversationAgentsReadPort + Clone + Send + Sync + 'static,
    D: LiveConversationRuntimeDelivery,
    S: Clone + Send + Sync + 'static,
{
    type Error = LaneEffectError;

    async fn announce(&self, changed: &WorkspaceMembershipChanged) -> Result<(), Self::Error> {
        let mut first = None;
        for notice in &changed.notices {
            if let Err(error) = self.announce_membership_notice(notice).await
                && first.is_none()
            {
                first = Some(error);
            }
            if let Err(error) = self
                .hub
                .publish_conversation_members_changed(&notice.conversation_id, &notice.workspace_id)
                && first.is_none()
            {
                first = Some(LaneEffectError::from_source(error));
            }
            if let Err(error) = self
                .hub
                .publish_workspace_channels_changed(&notice.workspace_id, &notice.conversation_id)
                && first.is_none()
            {
                first = Some(LaneEffectError::from_source(error));
            }
        }
        if let Some(viewer) = &changed.viewer
            && let Err(error) = self.announce_viewer(viewer).await
            && first.is_none()
        {
            first = Some(error);
        }
        first.map_or(Ok(()), Err)
    }

    async fn announce_viewer(
        &self,
        changed: &WorkspaceViewerMembershipChanged,
    ) -> Result<(), Self::Error> {
        let (change, role) = match changed.kind {
            WorkspaceViewerMembershipChangeKind::RoleChanged => {
                ("role_changed", changed.role.map(|role| role.as_str()))
            }
            WorkspaceViewerMembershipChangeKind::Removed => ("removed", None),
        };
        self.hub
            .publish_viewer_workspace_membership_changed(
                &changed.user_id,
                &changed.workspace_id,
                change,
                role,
            )
            .map_err(LaneEffectError::from_source)
    }
}

fn task_changed_payload(trigger: &TaskAftermathTrigger) -> Value {
    let task = trigger.committed();
    let kind = match task.kind {
        TaskEventKind::Create => "create",
        TaskEventKind::Claim => "claim",
        TaskEventKind::Assign => "assign",
        TaskEventKind::Unclaim => "unclaim",
        TaskEventKind::Status => "status",
        TaskEventKind::Edit => "edit",
    };
    json!({
        "kind": kind,
        "workspace_id": task.board.workspace_id().map(|id| id.as_str()),
        "thread_root_id": task.thread_root_id.as_ref().map(|id| id.as_str()),
        "conversation_id": task.conversation_id.as_str(),
        "task_number": task.task_number.get(),
        "task_id": task.task_id.as_str(),
        "source_message_id": task.source_message_id.as_ref().map(|id| id.as_str()),
        "status": task.status.as_str(),
        "assignee": task.assignee.as_ref().map(|value| value.to_json()),
        "title": task.title.as_str(),
        "closed_at": task.closed_at,
    })
}

fn task_message(trigger: &TaskAftermathTrigger) -> Result<Value, serde_json::Error> {
    let event = &trigger.committed().event_message;
    let metadata = serde_json::from_str::<serde_json::Map<String, Value>>(&event.message.metadata)?;
    Ok(json!({
        "id": event.id.as_str(),
        "conversation_id": event.conversation_id.as_str(),
        "from_kind": "system",
        "from_agent_id": Value::Null,
        "from_user_id": Value::Null,
        "author": Value::Null,
        "content": event.message.content,
        "metadata": Value::Object(metadata),
        "attachments": Value::Null,
        "reactions": Value::Null,
        "seq": event.seq,
        "created_at": event.created_at,
        "thread_root_id": event.thread_root_id.as_ref().map(|id| id.as_str()),
        "linked_task": Value::Null,
    }))
}

impl<U, A, G, D, S> TaskEffectExecute for LaneEffects<U, A, G, D, S>
where
    U: UnreadReadModel + Clone + Send + Sync + 'static,
    A: Clone + Send + Sync + 'static,
    G: ConversationAgentsReadPort + Clone + Send + Sync + 'static,
    D: LiveConversationRuntimeDelivery,
    S: Clone + Send + Sync + 'static,
{
    async fn execute_task_effects(&self, trigger: &TaskAftermathTrigger) -> Vec<String> {
        let mut failures = Vec::new();
        let task = trigger.committed();
        if trigger.plan().publish_event() {
            match task_message(trigger) {
                Ok(message) => {
                    if let Err(error) = self
                        .hub
                        .publish_system_message(&task.conversation_id, message)
                    {
                        failures.push(error.to_string());
                    }
                }
                Err(error) => failures.push(error.to_string()),
            }
        }
        let payload = task_changed_payload(trigger);
        if trigger.plan().patch_conversation_board()
            && let Err(error) = self
                .hub
                .publish_task_changed_to_conversation(&task.conversation_id, payload.clone())
        {
            failures.push(error.to_string());
        }
        if trigger.plan().patch_viewer_boards() {
            match self.unread.unread_audience(&task.conversation_id).await {
                Ok(Some(UnreadAudience::Workspace { viewers, .. })) => {
                    for viewer in viewers {
                        if let Err(error) = self
                            .hub
                            .publish_task_changed_to_viewer(&viewer.viewer, payload.clone())
                        {
                            failures.push(error.to_string());
                        }
                    }
                }
                Ok(Some(UnreadAudience::DirectPair { viewers })) => {
                    for viewer in viewers {
                        if let Err(error) = self
                            .hub
                            .publish_task_changed_to_viewer(&viewer, payload.clone())
                        {
                            failures.push(error.to_string());
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => failures.push(error.to_string()),
            }
        }
        if trigger.plan().wake_participants() {
            let committed_message_id = MessageId::parse(task.event_message.id.as_str())
                .expect("committed task event id is a message identity");
            match self.agents.agent_ids(&task.conversation_id).await {
                Ok(agents) => {
                    for agent in agents {
                        if trigger.plan().excluded_wake_agent() == Some(&agent) {
                            continue;
                        }
                        if let Err(error) = self
                            .delivery
                            .deliver_to_live_recipient(
                                &task.conversation_id,
                                &agent,
                                &committed_message_id,
                            )
                            .await
                        {
                            failures.push(error.to_string());
                        }
                    }
                }
                Err(error) => failures.push(error.to_string()),
            }
        }
        if let Some(notice) = trigger.assignee_notice()
            && let Err(error) = self.hub.publish_task_assignee_notice(notice, task)
        {
            failures.push(error.to_string());
        }
        if trigger.plan().cross_operation_audit()
            && let Some(audit) = trigger.cross_operation_audit()
        {
            tracing::warn!(
                op = "task_cross_op",
                source_conversation_id = audit.source().conversation_id().as_str(),
                source_workspace_id = audit.source().workspace_id().map(|id| id.as_str()),
                owner_conversation_id = audit.owner_conversation_id().as_str(),
                owner_workspace_id = audit.owner_workspace_id().map(|id| id.as_str()),
                task_number = task.task_number.get(),
                "task cross-conversation operation"
            );
        }
        failures
    }
}

/// The workspace-delete post-commit aftermath. It consumes the committed teardown
/// plan and never live-re-reads an audience the FK cascade has already erased:
/// the seats, blobs, and viewer members were all captured in the writer
/// transaction. It stops each captured conversation-scoped seat (resolving its live
/// host/generation by scope — the seat registry is in-memory, not cascade-affected),
/// best-effort deletes every captured attachment blob from the ONE blob store, and
/// fans out `viewer_workspaces_changed { removed }`. A single failed leg is logged
/// and never aborts the others — the 204 is already sealed.
#[derive(Clone)]
pub(super) struct ProductionWorkspaceDeletionAftermath<S, B, T, X> {
    hub: ScopeEventHub,
    terminalizer: T,
    runtime_stop: S,
    dispatch: X,
    blobs: B,
}

impl<S, B, T, X> ProductionWorkspaceDeletionAftermath<S, B, T, X> {
    pub(super) fn new(
        hub: ScopeEventHub,
        terminalizer: T,
        runtime_stop: S,
        dispatch: X,
        blobs: B,
    ) -> Self {
        Self {
            hub,
            terminalizer,
            runtime_stop,
            dispatch,
            blobs,
        }
    }
}

/// Bridge the concrete run-state terminalizer handle to the deletion aftermath's
/// narrow port lives in backend-server (orphan rule). See
/// `identity_access::workspace_deletion`.
impl<S, B, T, X>
    silverhand_backend_server::identity_access::workspace_deletion::WorkspaceDeletionAftermath
    for ProductionWorkspaceDeletionAftermath<S, B, T, X>
where
    S: MembershipRuntimeStop + Clone + Send + Sync + 'static,
    B: silverhand_backend_core::foundation::blob_store::BlobStore,
    T: silverhand_backend_server::identity_access::workspace_deletion::ConversationRunTerminalization,
    X: silverhand_backend_server::application_orchestration::agent_create::AftermathDispatch,
{
    async fn terminalize_and_sweep(
        &self,
        plan: &silverhand_backend_core::identity_access::workspace_deletion::WorkspaceTeardownPlan,
    ) {
        // Runs ONLY after phase 2 returned `Committed`: this call is the
        // authoritative deleter, so the destructive legs cannot affect a workspace it
        // does not own. Order mirrors TS `stopWorkspaceRuntime` then blob sweep:
        // (1) #44: terminalize the torn-down conversations' ConversationSlot runs to
        //     `conversation_closed` via the run-state projection authority, ONE
        //     `agent_activity` per run — a viewer sees the runs close immediately,
        //     not on the daemon's own later `runtime_exit`. Best-effort.
        // (2) #13: hand each daemon hard-stop to the owned executor DETACHED (a slow
        //     or never-resolving stop never gates the 204 or the blob sweep). This
        //     matches TS's `void stopWorkspaceRuntime(...)` detach.
        // (3) AWAIT the blob sweep over the captured refs. NB: the committed cascade
        //     (phase 2) already ran, so a mid-sweep crash leaves GC-able orphan blobs
        //     (a storage-GC concern) — deliberately traded for the #2 invariant that
        //     no destructive effect ever precedes the fresh committed authorization.
        // Terminalize over EVERY captured workspace conversation,
        // NOT a seat-derived set: a conversation whose roster is gone but whose run
        // map still has a live ConversationSlot run must still be closed.
        let conversations: std::collections::HashSet<ConversationId> =
            plan.conversation_ids.iter().cloned().collect();
        if !conversations.is_empty()
            && self
                .terminalizer
                .terminalize_conversations_closed(&conversations)
                .is_err()
        {
            tracing::warn!(
                workspace_id = plan.workspace.as_str(),
                "workspace-delete conversation_closed terminalization failed"
            );
        }
        // Detached hard-stops (#13): each seat's stop is an owned, tracked leg; a
        // failure is observed inside the leg and never blocks the 204/blob/viewer.
        for seat in &plan.runtime_seats {
            let runtime_stop = self.runtime_stop.clone();
            let conversation = seat.conversation.clone();
            let agent = seat.agent.clone();
            self.dispatch.dispatch(Box::pin(async move {
                if let Err(error) = runtime_stop.stop(&conversation, &agent).await {
                    tracing::warn!(
                        error = silverhand_backend_server::foundation::safe_log_field::safe_display(
                            &error
                        ),
                        conversation_id = conversation.as_str(),
                        agent_id = agent.as_str(),
                        "workspace-delete runtime stop failed"
                    );
                }
            }));
        }
        // Blob sweep: best-effort delete every captured attachment blob, over the
        // refs captured in phase 1 (the committed cascade already ran; see the #2
        // trade-off note above).
        for blob in &plan.blobs {
            if let Err(error) = self.blobs.delete(blob).await {
                tracing::warn!(
                    error =
                        silverhand_backend_server::foundation::safe_log_field::safe_display(&error),
                    workspace_id = plan.workspace.as_str(),
                    "workspace-delete attachment blob sweep failed"
                );
            }
        }
    }

    async fn announce_removed(
        &self,
        plan: &silverhand_backend_core::identity_access::workspace_deletion::WorkspaceTeardownPlan,
    ) {
        // Runs AFTER the row cascade: `viewer_workspaces_changed { removed }` per
        // captured member. A per-viewer failure is observable and does not abort the
        // rest; the 204 is already sealed.
        let publisher = self.hub.workspace_membership_publisher();
        for viewer in &plan.viewer_members {
            if let Err(error) = publisher.publish_workspace_removed(viewer, &plan.workspace) {
                tracing::warn!(
                    error =
                        silverhand_backend_server::foundation::safe_log_field::safe_display(&error),
                    workspace_id = plan.workspace.as_str(),
                    "workspace-delete viewer fan-out failed"
                );
            }
        }
    }
}

/// The production workspace-create onboarding aftermath: per agent, deliver the
/// onboarding message (awaited — the reference's critical leg), then hand a DETACHED
/// start leg to the owned effect executor (`AftermathDispatch`) and return
/// immediately. A slow/never-resolving start can never gate the `201` or the next
/// agent; a deliver failure returns `Err` so the route answers 500 and truncates.
#[derive(Clone)]
pub(super) struct ProductionWorkspaceCreationAftermath<OD, RS, X> {
    deliver: OD,
    start: RS,
    dispatch: X,
}

impl<OD, RS, X> ProductionWorkspaceCreationAftermath<OD, RS, X> {
    pub(super) fn new(deliver: OD, start: RS, dispatch: X) -> Self {
        Self {
            deliver,
            start,
            dispatch,
        }
    }
}

impl<OD, RS, X>
    silverhand_backend_server::identity_access::workspace_creation::WorkspaceCreationAftermath
    for ProductionWorkspaceCreationAftermath<OD, RS, X>
where
    OD: silverhand_backend_server::application_orchestration::agent_create::OnboardingDelivery,
    RS: silverhand_backend_core::conversation_messaging::messaging::post_commit::RuntimeStart
        + Clone
        + Send
        + Sync
        + 'static,
    X: silverhand_backend_server::application_orchestration::agent_create::AftermathDispatch,
{
    async fn deliver(
        &self,
        target: &silverhand_backend_core::identity_access::workspace_creation::WorkspaceOnboardingTarget,
        onboarding_message: &silverhand_backend_core::conversation_messaging::messaging::MessageId,
    ) -> Result<
        (),
        silverhand_backend_server::identity_access::workspace_creation::OnboardingDeliverFailed,
    > {
        self.deliver
            .deliver(&target.conversation, &target.agent, onboarding_message)
            .await
            .map_err(|error| {
                tracing::warn!(
                    error = silverhand_backend_server::foundation::safe_log_field::safe_display(&error),
                    conversation_id = target.conversation.as_str(),
                    agent_id = target.agent.as_str(),
                    "workspace-create onboarding deliver failed"
                );
                silverhand_backend_server::identity_access::workspace_creation::OnboardingDeliverFailed
            })
    }

    fn start(
        &self,
        target: &silverhand_backend_core::identity_access::workspace_creation::WorkspaceOnboardingTarget,
    ) {
        // Fire-and-forget through the OWNED executor (not a bare `tokio::spawn`): the
        // leg is tracked and cancellation-completed at shutdown. `start` returns the
        // instant the leg is dispatched, so a slow start never gates the 201 or the
        // next agent; the start's own failure is logged inside the leg.
        let start = self.start.clone();
        let conversation = target.conversation.clone();
        let agent = target.agent.clone();
        self.dispatch.dispatch(Box::pin(async move {
            if let Err(error) =
                silverhand_backend_core::conversation_messaging::messaging::post_commit::RuntimeStart::start(&start, &conversation, &agent).await
            {
                tracing::warn!(
                    error = silverhand_backend_server::foundation::safe_log_field::safe_display(&error),
                    conversation_id = conversation.as_str(),
                    agent_id = agent.as_str(),
                    "workspace-create onboarding start failed"
                );
            }
        }));
    }
}

/// The production create-channel fork aftermath: over the committed sealed
/// post-images, in STRICT order and each AWAITED before the route returns 200 —
/// (1) SYNC publish `workspace_channels_changed`, then (2) deliver the context seed
/// to the acting agent, (3) distribute the prompt to the OTHER member agents, and
/// (4) start ALL member runtimes. Delivery goes through the GENERIC committed-message
/// delivery authority (`LiveConversationRuntimeDelivery` over
/// `committed_message_inbox_ensure`), NOT the onboarding-only inbox-ensure whose shape
/// gate would reject a non-onboarding seed and leave the recipients with no inbox
/// custody + no daemon wake. The two seeds are already-committed
/// channel messages, so delivery is `deliver_to_live_recipient` (live participation),
/// not a sealed custody. The delivery/start AUDIENCE is the LIVE conversation roster
/// re-read at each effect point (prompt after the context deliver, start after the
/// prompt), NOT the committed fork roster: a recipient that left/joined
/// between commit and effect is excluded/included. Every leg after the publish is
/// best-effort: its failure is observed and logged but never rewrites the sealed 200
/// and never blocks a sibling leg. Unlike workspace-create (whose start is DETACHED
/// via `AftermathDispatch`), create-channel's start is an AWAITED best-effort gate — it
/// completes before the 200, matching TS `startAgentsBestEffort` inside the awaited
/// createChannel path (#40). The seed message payload/coordinates come from the
/// committed fork (no route-minted ids); only the audience is a live reread.
#[derive(Clone)]
pub(super) struct ProductionChannelForkAftermath<D, RS, G> {
    hub: ScopeEventHub,
    delivery: D,
    start: RS,
    roster: G,
}

impl<D, RS, G> ProductionChannelForkAftermath<D, RS, G> {
    pub(super) fn new(hub: ScopeEventHub, delivery: D, start: RS, roster: G) -> Self {
        Self {
            hub,
            delivery,
            start,
            roster,
        }
    }
}

impl<D, RS, G>
    silverhand_backend_server::conversation_messaging::messaging::collaboration::agent_create_channel::ChannelForkAftermath
    for ProductionChannelForkAftermath<D, RS, G>
where
    D: silverhand_backend_server::conversation_messaging::LiveConversationRuntimeDelivery
        + Send
        + Sync
        + 'static,
    RS: silverhand_backend_core::conversation_messaging::messaging::post_commit::RuntimeStart
        + Send
        + Sync
        + 'static,
    G: silverhand_backend_core::conversation_messaging::messaging::collaboration::workspace_rename_messaging::ConversationAgentsReadPort
        + Send
        + Sync
        + 'static,
{
    async fn channel_forked(
        &self,
        workspace: &WorkspaceId,
        committed: &silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_fork::CommittedChannelFork,
    ) {
        use silverhand_backend_server::foundation::safe_log_field::safe_display;

        let channel = &committed.channel;

        // (1) SYNC invalidation: the new channel joins the workspace's channel set.
        // A publish failure is observable; it never rewrites the sealed 200.
        if let Err(error) = self
            .hub
            .publish_workspace_channels_changed(workspace, channel)
        {
            tracing::warn!(
                error = safe_display(&error),
                workspace_id = workspace.as_str(),
                conversation_id = channel.as_str(),
                "channel-fork workspace_channels_changed publish failed"
            );
        }

        // (2) Deliver the context seed to the ACTING agent (awaited best-effort). The
        // actor is the creator; delivery ensures its durable inbox custody for the
        // committed context message + wakes it.
        if let Err(error) = self
            .delivery
            .deliver_to_live_recipient(channel, &committed.actor_agent, &committed.context_message.id)
            .await
        {
            tracing::warn!(
                error = safe_display(&error),
                conversation_id = channel.as_str(),
                agent_id = committed.actor_agent.as_str(),
                "channel-fork context deliver to actor failed"
            );
        }

        // (3) Distribute the prompt to the OTHER member agents — the audience is the
        // LIVE roster re-read at effect time, NOT the committed set: TS
        // calls `listAgents(channel)` after the context deliver, so an agent that LEFT
        // between commit and now is not prompted and one that JOINED is. The committed
        // roster is only the creation fact. A read failure logs and skips this leg (the
        // committed roster is not a safe fallback — it would resurrect the stale set).
        match self.roster.agent_ids(channel).await {
            Ok(prompt_audience) => {
                for agent in &prompt_audience {
                    if agent == &committed.actor_agent {
                        continue;
                    }
                    if let Err(error) = self
                        .delivery
                        .deliver_to_live_recipient(channel, agent, &committed.prompt_message.id)
                        .await
                    {
                        tracing::warn!(
                            error = safe_display(&error),
                            conversation_id = channel.as_str(),
                            agent_id = agent.as_str(),
                            "channel-fork prompt distribution failed"
                        );
                    }
                }
            }
            Err(error) => {
                tracing::warn!(
                    error = safe_display(&error),
                    conversation_id = channel.as_str(),
                    "channel-fork prompt-audience live roster read failed"
                );
            }
        }

        // (4) Start ALL member runtimes — again the LIVE roster re-read AFTER the prompt:
        // TS calls `listAgents(channel)` a second time for the start
        // audience, so a left agent is never started and a joined one is. Awaited
        // best-effort gate before the 200; one start's failure does not block the rest.
        match self.roster.agent_ids(channel).await {
            Ok(start_audience) => {
                for agent in &start_audience {
                    if let Err(error) =
                        silverhand_backend_core::conversation_messaging::messaging::post_commit::RuntimeStart::start(
                            &self.start, channel, agent,
                        )
                        .await
                    {
                        tracing::warn!(
                            error = safe_display(&error),
                            conversation_id = channel.as_str(),
                            agent_id = agent.as_str(),
                            "channel-fork runtime start failed"
                        );
                    }
                }
            }
            Err(error) => {
                tracing::warn!(
                    error = safe_display(&error),
                    conversation_id = channel.as_str(),
                    "channel-fork start-audience live roster read failed"
                );
            }
        }
    }
}

/// The reserved process clock for the create saga's row stamps.
#[derive(Clone, Copy)]
pub(super) struct SystemCreationClock;

impl silverhand_backend_core::identity_access::workspace_creation::WorkspaceProvisionClock
    for SystemCreationClock
{
    fn now_ms(&self) -> i64 {
        // The one process wall-clock owner, mirroring every other signed-millis
        // consumer; `SystemProcessClock` reads it once per call.
        use silverhand_backend_server::foundation::clock::ProcessClock;
        silverhand_backend_server::foundation::clock::SystemProcessClock.now_unix_millis()
    }
}

impl silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_fork::ChannelForkClock
    for SystemCreationClock
{
    fn now_ms(&self) -> i64 {
        use silverhand_backend_server::foundation::clock::ProcessClock;
        silverhand_backend_server::foundation::clock::SystemProcessClock.now_unix_millis()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use silverhand_backend_core::conversation_messaging::messaging::{
        MessageId,
        collaboration::workspace_rename_messaging::ConversationAgentsReadPort,
        post_commit::EffectFailure,
        task::{
            CreatedEventMessageId, TaskCommittedEventMessage, TaskEventMessage, TaskId,
            TaskMutationWitness, TaskNumber, TaskStatus, TaskTitle,
        },
    };

    use super::*;

    #[derive(Clone)]
    struct Agents(AgentId);

    impl ConversationAgentsReadPort for Agents {
        type Error = Infallible;

        async fn agent_ids(&self, _: &ConversationId) -> Result<Vec<AgentId>, Self::Error> {
            Ok(vec![self.0.clone()])
        }
    }

    #[derive(Clone, Default)]
    struct Delivery(Arc<AtomicUsize>);

    impl LiveConversationRuntimeDelivery for Delivery {
        async fn deliver_to_live_recipient(
            &self,
            _: &ConversationId,
            _: &AgentId,
            _: &MessageId,
        ) -> Result<(), EffectFailure> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    impl SealedConversationRuntimeDelivery for Delivery {
        async fn deliver_to_sealed_recipient(
            &self,
            _: &ConversationId,
            _: &AgentId,
            _: &MessageId,
        ) -> Result<(), EffectFailure> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[derive(Clone)]
    struct Unread;

    impl UnreadReadModel for Unread {
        async fn unread_audience(
            &self,
            _: &ConversationId,
        ) -> Result<Option<UnreadAudience>, PersistenceFailure> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn malformed_membership_metadata_is_observable_and_does_not_skip_delivery() {
        let delivery = Delivery::default();
        let effects = LaneEffects::new(
            ScopeEventHub::production().unwrap(),
            (),
            (),
            Agents(AgentId::parse("recipient").unwrap()),
            delivery.clone(),
            (),
        );
        for (index, metadata) in ["{not-json", "null", "[]", "\"scalar\""]
            .into_iter()
            .enumerate()
        {
            let notice = CommittedMembershipNotice {
                conversation_id: ConversationId::parse("conversation").unwrap(),
                workspace_id: WorkspaceId::parse("workspace").unwrap(),
                message_id: MessageId::parse(format!("message-{index}")).unwrap(),
                content: "committed".to_owned(),
                metadata: metadata.to_owned(),
                seq: 1,
                created_at: 1,
            };

            assert!(effects.announce_membership_notice(&notice).await.is_err());
            assert!(membership_message(&notice).is_err());
        }
        assert_eq!(delivery.0.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn malformed_task_metadata_is_observable_and_does_not_skip_live_wake() {
        let delivery = Delivery::default();
        let effects = LaneEffects::new(
            ScopeEventHub::production().unwrap(),
            Unread,
            (),
            Agents(AgentId::parse("recipient").unwrap()),
            delivery.clone(),
            (),
        );
        let conversation = ConversationId::parse("conversation").unwrap();
        for (index, metadata) in ["{not-json", "null", "[]", "\"scalar\""]
            .into_iter()
            .enumerate()
        {
            let witness = TaskMutationWitness {
                kind: TaskEventKind::Create,
                task_id: TaskId::parse(format!("task-{index}")).unwrap(),
                task_number: TaskNumber::new(index as i64 + 1).unwrap(),
                event_message: TaskCommittedEventMessage::new(
                    CreatedEventMessageId::parse(format!("message-{index}")).unwrap(),
                    conversation.clone(),
                    None,
                    TaskEventMessage {
                        content: "committed".to_owned(),
                        metadata: metadata.to_owned(),
                    },
                    1,
                    1,
                ),
                conversation_id: conversation.clone(),
                board: BoardKey::Workspace(WorkspaceId::parse("workspace").unwrap()),
                thread_root_id: None,
                title: TaskTitle::parse("task").unwrap(),
                status: TaskStatus::Todo,
                assignee: None,
                closed_at: None,
                source_message_id: None,
            };
            let trigger =
                TaskAftermathTrigger::for_human(witness, UserId::parse("viewer").unwrap());

            let failures = effects.execute_task_effects(&trigger).await;
            assert!(!failures.is_empty());
            assert!(task_message(&trigger).is_err());
        }
        assert_eq!(delivery.0.load(Ordering::SeqCst), 4);
    }

    // ── Phase B P0: workspace-delete aftermath consumes the committed plan ────────
    use silverhand_backend_core::{
        foundation::blob_store::{BlobNamespace, BlobStore},
        identity_access::workspace_deletion::{WorkspaceRuntimeSeat, WorkspaceTeardownPlan},
    };
    use silverhand_backend_server::identity_access::workspace_deletion::WorkspaceDeletionAftermath;

    /// A runtime stop that records every `(conversation, agent)` it is asked to stop
    /// and fails for one configured seat — proving a single failed leg is observable
    /// and does not abort the remaining seats or blob sweep.
    #[derive(Clone)]
    struct RecordingStop {
        stopped: Arc<std::sync::Mutex<Vec<(String, String)>>>,
        fail_agent: Option<String>,
    }

    impl MembershipRuntimeStop for RecordingStop {
        type Error = LaneEffectError;
        async fn stop(
            &self,
            conversation: &ConversationId,
            agent: &AgentId,
        ) -> Result<(), Self::Error> {
            self.stopped
                .lock()
                .unwrap()
                .push((conversation.as_str().to_owned(), agent.as_str().to_owned()));
            if self.fail_agent.as_deref() == Some(agent.as_str()) {
                return Err(LaneEffectError("configured seat failure".into()));
            }
            Ok(())
        }
    }

    /// A run-state terminalizer stub that records the conversation set it was asked
    /// to close.
    #[derive(Clone, Default)]
    struct RecordingTerminalizer {
        closed: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl silverhand_backend_server::identity_access::workspace_deletion::ConversationRunTerminalization
        for RecordingTerminalizer
    {
        fn terminalize_conversations_closed(
            &self,
            conversations: &std::collections::HashSet<ConversationId>,
        ) -> Result<
            (),
            silverhand_backend_server::identity_access::workspace_deletion::ConversationRunTerminalizeError,
        > {
            let mut seen: Vec<String> =
                conversations.iter().map(|c| c.as_str().to_owned()).collect();
            seen.sort();
            *self.closed.lock().unwrap() = seen;
            Ok(())
        }
    }

    /// An `AftermathDispatch` stub that collects the detached legs so the test can
    /// drive them to completion deterministically (production hands them to the owned
    /// executor).
    #[derive(Clone, Default)]
    struct CollectingDispatch {
        legs: Arc<
            std::sync::Mutex<
                Vec<
                    silverhand_backend_server::application_orchestration::agent_create::AftermathLeg,
                >,
            >,
        >,
    }

    impl silverhand_backend_server::application_orchestration::agent_create::AftermathDispatch
        for CollectingDispatch
    {
        fn dispatch(
            &self,
            leg: silverhand_backend_server::application_orchestration::agent_create::AftermathLeg,
        ) {
            self.legs.lock().unwrap().push(leg);
        }
    }

    #[tokio::test]
    async fn workspace_delete_aftermath_stops_captured_seats_and_sweeps_blobs_without_rereading() {
        // A real local blob store with two live blobs; the plan references both.
        let dir = tempfile::tempdir().unwrap();
        let blobs =
            silverhand_backend_filesystem::foundation::blob_store::LocalBlobStore::open(dir.path())
                .await
                .unwrap();
        let namespace = BlobNamespace::from_segments(["attachments"]).unwrap();
        let kept = blobs
            .put(&namespace, "text/plain", b"one".to_vec())
            .await
            .unwrap();
        let swept = blobs
            .put(&namespace, "text/plain", b"two".to_vec())
            .await
            .unwrap();
        // Prove both files exist before the sweep.
        assert!(matches!(
            blobs.read(&kept).await.unwrap(),
            silverhand_backend_core::foundation::blob_store::BlobReadOutcome::Found(_)
        ));
        assert!(matches!(
            blobs.read(&swept).await.unwrap(),
            silverhand_backend_core::foundation::blob_store::BlobReadOutcome::Found(_)
        ));

        let stopped = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stop = RecordingStop {
            stopped: Arc::clone(&stopped),
            // The FIRST captured seat fails; the second must still be attempted.
            fail_agent: Some("agent-1".to_owned()),
        };
        let terminalizer = RecordingTerminalizer::default();
        let dispatch = CollectingDispatch::default();
        let aftermath = ProductionWorkspaceDeletionAftermath::new(
            ScopeEventHub::production().unwrap(),
            terminalizer.clone(),
            stop,
            dispatch.clone(),
            blobs.clone(),
        );

        let plan = WorkspaceTeardownPlan {
            runtime_seats: vec![
                WorkspaceRuntimeSeat {
                    conversation: ConversationId::parse("conv-a").unwrap(),
                    agent: AgentId::parse("agent-1").unwrap(),
                },
                WorkspaceRuntimeSeat {
                    conversation: ConversationId::parse("conv-b").unwrap(),
                    agent: AgentId::parse("agent-2").unwrap(),
                },
            ],
            blobs: vec![kept.clone(), swept.clone()],
            // conv-c has NO routed seat (its roster is gone) but IS a workspace
            // conversation — the independently-captured set must still terminalize it.
            conversation_ids: vec![
                ConversationId::parse("conv-a").unwrap(),
                ConversationId::parse("conv-b").unwrap(),
                ConversationId::parse("conv-c").unwrap(),
            ],
            viewer_members: vec![UserId::parse("user-1").unwrap()],
            workspace: WorkspaceId::parse("ws-1").unwrap(),
        };

        aftermath.terminalize_and_sweep(&plan).await;

        // #44/#2: the run-state projection was asked to close EVERY captured workspace
        // conversation — including conv-c which has NO routed seat (roster gone) — not
        // just the seat-derived set.
        assert_eq!(
            terminalizer.closed.lock().unwrap().clone(),
            vec![
                "conv-a".to_owned(),
                "conv-b".to_owned(),
                "conv-c".to_owned()
            ],
        );

        // #13: the hard-stops are DETACHED legs handed to the owned executor — drive
        // the collected legs to completion (production's task tracker does this).
        let legs = {
            let mut guard = dispatch.legs.lock().unwrap();
            std::mem::take(&mut *guard)
        };
        assert_eq!(legs.len(), 2, "both seats' stops were dispatched detached");
        for leg in legs {
            leg.await;
        }

        // Both captured seats were attempted — the first seat's failure did NOT abort
        // the second. The audience came from the committed plan, never a post-cascade
        // reread. Detached order is not contractual, so assert as a set.
        let mut seats = stopped.lock().unwrap().clone();
        seats.sort();
        assert_eq!(
            seats,
            vec![
                ("conv-a".to_owned(), "agent-1".to_owned()),
                ("conv-b".to_owned(), "agent-2".to_owned()),
            ]
        );
        // Both blobs were swept from the one blob store — physical files gone.
        assert!(matches!(
            blobs.read(&kept).await.unwrap(),
            silverhand_backend_core::foundation::blob_store::BlobReadOutcome::Missing
        ));
        assert!(matches!(
            blobs.read(&swept).await.unwrap(),
            silverhand_backend_core::foundation::blob_store::BlobReadOutcome::Missing
        ));
    }

    // ===================================================================
    // #40 create-channel awaited aftermath: deliver(context→actor) →
    // distribute(prompt→others) → start(all), each awaited best-effort.
    // ===================================================================

    #[derive(Clone, Default)]
    struct RecordingDelivery {
        // (agent, message_id) pairs, in the order deliver was invoked.
        calls: Arc<std::sync::Mutex<Vec<(String, String)>>>,
        // Agents whose deliver returns Err (to prove non-blocking).
        fail_agent: Option<String>,
    }

    impl silverhand_backend_server::conversation_messaging::LiveConversationRuntimeDelivery
        for RecordingDelivery
    {
        async fn deliver_to_live_recipient(
            &self,
            _conversation: &ConversationId,
            agent: &AgentId,
            message: &MessageId,
        ) -> Result<(), EffectFailure> {
            self.calls
                .lock()
                .unwrap()
                .push((agent.as_str().to_owned(), message.as_str().to_owned()));
            if self.fail_agent.as_deref() == Some(agent.as_str()) {
                return Err(EffectFailure::from_source(std::io::Error::other("boom")));
            }
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct RecordingStart {
        starts: Arc<std::sync::Mutex<Vec<String>>>,
        fail_agent: Option<String>,
    }

    impl silverhand_backend_core::conversation_messaging::messaging::post_commit::RuntimeStart
        for RecordingStart
    {
        async fn start(
            &self,
            _conversation: &ConversationId,
            agent: &AgentId,
        ) -> Result<(), EffectFailure> {
            self.starts.lock().unwrap().push(agent.as_str().to_owned());
            if self.fail_agent.as_deref() == Some(agent.as_str()) {
                return Err(EffectFailure::from_source(std::io::Error::other("boom")));
            }
            Ok(())
        }
    }

    // A `ConversationAgentsReadPort` whose successive `agent_ids` calls return the
    // QUEUED live-roster snapshots in order — modelling a roster that mutates
    // between the prompt read and the start read. When the queue is
    // exhausted it yields an empty live roster (NOT a resurrected committed set);
    // `fail` forces Err on every read to prove the committed roster is not a fallback.
    #[derive(Clone)]
    struct RecordingAgents {
        // conversation log so a test can prove the reads happened + how many.
        reads: Arc<std::sync::Mutex<Vec<String>>>,
        // FIFO of roster snapshots — one dequeued per `agent_ids` call.
        snapshots: Arc<std::sync::Mutex<std::collections::VecDeque<Vec<String>>>>,
        fail: bool,
    }

    impl RecordingAgents {
        fn with(snapshots: Vec<Vec<&str>>) -> Self {
            Self {
                reads: Arc::default(),
                snapshots: Arc::new(std::sync::Mutex::new(
                    snapshots
                        .into_iter()
                        .map(|s| s.into_iter().map(str::to_owned).collect())
                        .collect(),
                )),
                fail: false,
            }
        }
    }

    impl silverhand_backend_core::conversation_messaging::messaging::collaboration::workspace_rename_messaging::ConversationAgentsReadPort
        for RecordingAgents
    {
        type Error = std::io::Error;

        async fn agent_ids(
            &self,
            conversation: &ConversationId,
        ) -> Result<Vec<AgentId>, Self::Error> {
            self.reads
                .lock()
                .unwrap()
                .push(conversation.as_str().to_owned());
            if self.fail {
                return Err(std::io::Error::other("roster read boom"));
            }
            let snapshot = self.snapshots.lock().unwrap().pop_front();
            match snapshot {
                Some(ids) => Ok(ids
                    .into_iter()
                    .map(|id| AgentId::parse(&id).unwrap())
                    .collect()),
                None => Ok(vec![]),
            }
        }
    }

    fn committed_fork(
        channel: &str,
        actor: &str,
        others: &[&str],
    ) -> silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_fork::CommittedChannelFork{
        use silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_fork::{
            CommittedChannelFork, SealedSeedMessage,
        };
        let chan = ConversationId::parse(channel).unwrap();
        let mut agents = vec![AgentId::parse(actor).unwrap()];
        agents.extend(others.iter().map(|a| AgentId::parse(*a).unwrap()));
        CommittedChannelFork {
            channel: chan.clone(),
            actor_agent: AgentId::parse(actor).unwrap(),
            context_message: SealedSeedMessage {
                id: MessageId::parse("msg-context").unwrap(),
                conversation: chan.clone(),
                seq: 1,
                content: "ctx".into(),
            },
            prompt_message: SealedSeedMessage {
                id: MessageId::parse("msg-prompt").unwrap(),
                conversation: chan,
                seq: 2,
                content: "prompt".into(),
            },
            member_agents: agents,
            member_users: vec![],
        }
    }

    #[tokio::test]
    async fn channel_fork_aftermath_delivers_context_to_actor_prompt_to_others_and_starts_all() {
        use silverhand_backend_server::conversation_messaging::messaging::collaboration::agent_create_channel::ChannelForkAftermath;
        use tokio::sync::oneshot;

        // These gates suspend real dependency futures after entry. Only the actual
        // aftermath can advance to the next phase; the test never supplies its order.
        #[derive(Clone)]
        struct GatedDelivery {
            recorded: RecordingDelivery,
            context: Arc<std::sync::Mutex<Option<oneshot::Receiver<()>>>>,
            prompt: Arc<std::sync::Mutex<Option<oneshot::Receiver<()>>>>,
        }
        impl LiveConversationRuntimeDelivery for GatedDelivery {
            async fn deliver_to_live_recipient(
                &self,
                conversation: &ConversationId,
                agent: &AgentId,
                message: &MessageId,
            ) -> Result<(), EffectFailure> {
                self.recorded
                    .deliver_to_live_recipient(conversation, agent, message)
                    .await?;
                let gate = if message.as_str() == "msg-context" {
                    self.context.lock().unwrap().take()
                } else {
                    self.prompt.lock().unwrap().take()
                };
                if let Some(gate) = gate {
                    gate.await.unwrap();
                }
                Ok(())
            }
        }
        struct GatedStart {
            recorded: RecordingStart,
            last: std::sync::Mutex<Option<oneshot::Receiver<()>>>,
        }
        impl silverhand_backend_core::conversation_messaging::messaging::post_commit::RuntimeStart
            for GatedStart
        {
            async fn start(
                &self,
                conversation: &ConversationId,
                agent: &AgentId,
            ) -> Result<(), EffectFailure> {
                self.recorded.start(conversation, agent).await?;
                let gate = if agent.as_str() == "agent-c" {
                    self.last.lock().unwrap().take()
                } else {
                    None
                };
                if let Some(gate) = gate {
                    gate.await.unwrap();
                }
                Ok(())
            }
        }

        let delivery = RecordingDelivery::default();
        let start = RecordingStart::default();
        let (release_context, context) = oneshot::channel();
        let (release_prompt, prompt) = oneshot::channel();
        let (release_start, last) = oneshot::channel();
        let hub = ScopeEventHub::production().unwrap();
        // Live roster is unchanged from the committed set across both reads.
        let roster = RecordingAgents::with(vec![
            vec!["agent-actor", "agent-b", "agent-c"],
            vec!["agent-actor", "agent-b", "agent-c"],
        ]);
        let aftermath = ProductionChannelForkAftermath::new(
            hub,
            GatedDelivery {
                recorded: delivery.clone(),
                context: Arc::new(std::sync::Mutex::new(Some(context))),
                prompt: Arc::new(std::sync::Mutex::new(Some(prompt))),
            },
            GatedStart {
                recorded: start.clone(),
                last: std::sync::Mutex::new(Some(last)),
            },
            roster.clone(),
        );

        let committed = committed_fork("conv-chan", "agent-actor", &["agent-b", "agent-c"]);
        let workspace = WorkspaceId::parse("ws-1").unwrap();
        let pending = aftermath.channel_forked(&workspace, &committed);
        tokio::pin!(pending);
        assert!(futures_util::poll!(&mut pending).is_pending());
        assert_eq!(delivery.calls.lock().unwrap().len(), 1);
        assert!(roster.reads.lock().unwrap().is_empty());
        assert!(start.starts.lock().unwrap().is_empty());

        release_context.send(()).unwrap();
        assert!(futures_util::poll!(&mut pending).is_pending());
        assert_eq!(delivery.calls.lock().unwrap().len(), 2);
        assert_eq!(roster.reads.lock().unwrap().len(), 1);
        assert!(start.starts.lock().unwrap().is_empty());

        release_prompt.send(()).unwrap();
        assert!(futures_util::poll!(&mut pending).is_pending());
        assert_eq!(delivery.calls.lock().unwrap().len(), 3);
        assert_eq!(roster.reads.lock().unwrap().len(), 2);
        assert_eq!(start.starts.lock().unwrap().len(), 3);
        release_start.send(()).unwrap();
        pending.await;

        // The live roster was read exactly TWICE (once for the prompt audience after
        // the context deliver, once for the start audience after the prompt) — the two
        // `listAgents` calls the production TS makes, not the committed set.
        assert_eq!(
            roster.reads.lock().unwrap().clone(),
            vec!["conv-chan".to_owned(), "conv-chan".to_owned()],
        );

        // Context seed → actor first; prompt → each OTHER agent (never the actor).
        let calls = delivery.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![
                ("agent-actor".to_owned(), "msg-context".to_owned()),
                ("agent-b".to_owned(), "msg-prompt".to_owned()),
                ("agent-c".to_owned(), "msg-prompt".to_owned()),
            ],
            "context goes to the actor, prompt is distributed to the OTHER members only, in order"
        );

        // Start-all covers every member agent (including the actor).
        let starts = start.starts.lock().unwrap().clone();
        assert_eq!(
            starts,
            vec![
                "agent-actor".to_owned(),
                "agent-b".to_owned(),
                "agent-c".to_owned()
            ],
            "every member runtime is started"
        );
    }

    #[tokio::test]
    async fn channel_fork_aftermath_one_leg_failure_does_not_block_siblings() {
        use silverhand_backend_server::conversation_messaging::messaging::collaboration::agent_create_channel::ChannelForkAftermath;

        // The actor's context deliver fails AND agent-b's start fails: the remaining
        // deliver + start legs must still all run (best-effort, non-blocking).
        let delivery = RecordingDelivery {
            calls: Arc::default(),
            fail_agent: Some("agent-actor".to_owned()),
        };
        let start = RecordingStart {
            starts: Arc::default(),
            fail_agent: Some("agent-b".to_owned()),
        };
        let hub = ScopeEventHub::production().unwrap();
        // Live roster unchanged (both reads return the full committed set); this test
        // isolates best-effort non-blocking, not the roster axis.
        let roster = RecordingAgents::with(vec![
            vec!["agent-actor", "agent-b", "agent-c"],
            vec!["agent-actor", "agent-b", "agent-c"],
        ]);
        let aftermath =
            ProductionChannelForkAftermath::new(hub, delivery.clone(), start.clone(), roster);

        let committed = committed_fork("conv-chan", "agent-actor", &["agent-b", "agent-c"]);
        let workspace = WorkspaceId::parse("ws-1").unwrap();
        aftermath.channel_forked(&workspace, &committed).await;

        // A failed context deliver to the actor did NOT abort the prompt distribution.
        let delivered_agents: Vec<String> = delivery
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|(a, _)| a.clone())
            .collect();
        assert_eq!(
            delivered_agents,
            vec!["agent-actor", "agent-b", "agent-c"],
            "a failed leg is observed but the remaining deliver legs still run"
        );
        // A failed start for agent-b did NOT abort agent-c's start.
        assert_eq!(
            start.starts.lock().unwrap().clone(),
            vec!["agent-actor", "agent-b", "agent-c"],
            "a failed start is observed but the remaining start legs still run"
        );
    }

    // ===================================================================
    // The prompt audience and the start audience are each
    // the LIVE roster re-read at their own effect point, NOT the committed set.
    // A member that LEFT between commit and the prompt read is neither prompted
    // nor started; a member that JOINED is. Because the start read happens AFTER
    // the prompt, a change landing in that window is reflected in start only.
    // If either phase iterated `committed.member_agents`, this fails by name.
    // ===================================================================
    #[tokio::test]
    async fn channel_fork_prompt_and_start_each_use_their_own_live_roster_not_committed() {
        use silverhand_backend_server::conversation_messaging::messaging::collaboration::agent_create_channel::ChannelForkAftermath;

        let delivery = RecordingDelivery::default();
        let start = RecordingStart::default();
        let hub = ScopeEventHub::production().unwrap();

        // Committed creation fact = {actor, b, c}. But by the time the aftermath runs:
        //   • prompt read (1st): agent-b LEFT, agent-d JOINED  → live {actor, c, d}
        //   • start  read (2nd): agent-c also LEFT             → live {actor, d}
        // committed b must never be prompted or started (stale); joined d must be both.
        let roster = RecordingAgents::with(vec![
            vec!["agent-actor", "agent-c", "agent-d"],
            vec!["agent-actor", "agent-d"],
        ]);
        let aftermath = ProductionChannelForkAftermath::new(
            hub,
            delivery.clone(),
            start.clone(),
            roster.clone(),
        );

        let committed = committed_fork("conv-chan", "agent-actor", &["agent-b", "agent-c"]);
        let workspace = WorkspaceId::parse("ws-1").unwrap();
        aftermath.channel_forked(&workspace, &committed).await;

        // Prompt distribution used the FIRST live read (minus the actor): c + d, NOT the
        // committed b. Context still goes to the actor first (unconditional creator leg).
        let calls = delivery.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![
                ("agent-actor".to_owned(), "msg-context".to_owned()),
                ("agent-c".to_owned(), "msg-prompt".to_owned()),
                ("agent-d".to_owned(), "msg-prompt".to_owned()),
            ],
            "prompt audience = live roster at the first read (joined d in, left b out), never committed b"
        );

        // Start used the SECOND live read: actor + d only. Left c (gone after the prompt
        // read) is NOT started; committed b is never started.
        assert_eq!(
            start.starts.lock().unwrap().clone(),
            vec!["agent-actor".to_owned(), "agent-d".to_owned()],
            "start audience = live roster at the second read, never the committed set"
        );

        // Both reads were against the channel, in order.
        assert_eq!(
            roster.reads.lock().unwrap().clone(),
            vec!["conv-chan".to_owned(), "conv-chan".to_owned()],
        );
    }

    // ===================================================================
    // fallback ban: if the live roster read FAILS, that phase is
    // skipped — the committed set is NOT resurrected as a fallback (which would
    // prompt/start a stale roster). A roster read error logs and drops the leg.
    // ===================================================================
    #[tokio::test]
    async fn channel_fork_roster_read_failure_does_not_fall_back_to_committed() {
        use silverhand_backend_server::conversation_messaging::messaging::collaboration::agent_create_channel::ChannelForkAftermath;

        let delivery = RecordingDelivery::default();
        let start = RecordingStart::default();
        let hub = ScopeEventHub::production().unwrap();
        let roster = RecordingAgents {
            reads: Arc::default(),
            snapshots: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            fail: true,
        };
        let aftermath =
            ProductionChannelForkAftermath::new(hub, delivery.clone(), start.clone(), roster);

        let committed = committed_fork("conv-chan", "agent-actor", &["agent-b", "agent-c"]);
        let workspace = WorkspaceId::parse("ws-1").unwrap();
        aftermath.channel_forked(&workspace, &committed).await;

        // Only the unconditional actor context deliver ran; NO prompt was distributed to
        // the committed b/c (no fallback), and NO runtime was started from committed.
        assert_eq!(
            delivery.calls.lock().unwrap().clone(),
            vec![("agent-actor".to_owned(), "msg-context".to_owned())],
            "a failed prompt-audience read drops the prompt leg — committed roster is not a fallback"
        );
        assert!(
            start.starts.lock().unwrap().is_empty(),
            "a failed start-audience read starts nobody — committed roster is not a fallback"
        );
    }
}
