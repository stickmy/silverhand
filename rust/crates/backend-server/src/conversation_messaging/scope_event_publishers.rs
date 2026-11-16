//! Typed scope-event publishers owned by conversation and messaging capabilities.

use serde_json::json;
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        CommittedMessageView, CreatedMessage, DirectPairOpenedFact, DirectPairOpenedPort,
        ParticipantHandle, PrincipalIdentity,
        alert::{AlertPlace, AlertReason, MessageAlertAudience, alert_reason},
        collaboration::{
            attachment::{AttachmentChangedFact, AttachmentChangedPublisher},
            conversation_latest::ConversationLatest,
            notification::{
                CommittedNotificationClear, NotificationAftermath, NotificationClearFailure,
            },
            reaction::{ReactionAftermath, ReactionChangedFact, ReactionPublicationFailure},
            saved::{SavedAftermath, SavedChangeFact, SavedPublicationFailure},
            thread_follow::{
                ThreadFollowAftermath, ThreadFollowChangedFact, ThreadFollowPublicationFailure,
            },
            workspace_rename_messaging::{
                BoundWorkspaceRenameMessage, WorkspaceRenamePublicationPort,
            },
        },
        mention_cc::{CommittedMentionCopy, mention_excerpt},
        post_commit::{
            AlertPublication, CommittedMentionRow, CommittedMessagePublication, CopyNotification,
            DirectLineInvalidation, EffectFailure, LatestPublication, MessagePublication,
            UnreadPublication, ViewerNotification,
        },
        unread::{
            ConversationHumanAudience, LatestUnread as CoreLatestUnread, UnreadAudience,
            UnreadReadModel, ViewerUnread,
        },
    },
    foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId},
    foundation::scope_event::EventScope,
};

use crate::{
    conversation_messaging::messaging::{
        aftermath::MessagingAftermath,
        author_wire::author_dto,
        collaboration::{
            attachment::changed_event as attachment_changed_payload,
            conversation_latest::latest_dto, reaction::changed_event as reaction_changed_payload,
            saved::changed_event as saved_changed_payload,
        },
        message_wire::{copy_dto, message_event_payload, rename_dto},
    },
    foundation::{
        scope_event_transport::{ScopeEventHub, ScopeEventHubError},
        scope_event_wire::{
            AttentionBadge, LatestUnread, NotificationEvent, NotificationKind, ProductEvent,
            UnreadUpdated,
        },
    },
};

#[derive(Clone)]
pub struct HubMessagingEvents {
    hub: ScopeEventHub,
}

impl HubMessagingEvents {
    fn new(hub: ScopeEventHub) -> Self {
        Self { hub }
    }

    fn publish_direct_pairs_changed(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<(), ScopeEventHubError> {
        self.hub.publish_direct_pairs_changed(viewer, conversation)
    }

    fn publish(&self, scope: EventScope, event: ProductEvent) -> Result<(), ScopeEventHubError> {
        self.hub.publish(scope, event)
    }
}

#[derive(Clone)]
pub struct HubWorkspaceRenamePublisher(ScopeEventHub);

#[derive(Clone)]
pub struct HubSavedPublisher(ScopeEventHub);

#[derive(Clone)]
pub struct HubAttachmentPublisher(ScopeEventHub);

#[derive(Clone)]
pub struct HubReactionPublisher(ScopeEventHub);

impl ScopeEventHub {
    pub fn messaging_events(&self) -> HubMessagingEvents {
        HubMessagingEvents::new(self.clone())
    }

    /// Builds the messaging aftermath over this hub. The adapter constructor is
    /// intentionally unavailable across crates, so callers cannot quietly bind the
    /// unread publisher to a second hub.
    ///
    /// ```compile_fail
    /// use silverhand_backend_server::{
    ///     conversation_messaging::scope_event_publishers::HubUnreadPublisher,
    ///     foundation::scope_event_transport::ScopeEventHub,
    /// };
    ///
    /// let hub = ScopeEventHub::production().unwrap();
    /// let _ = HubUnreadPublisher::new(hub, ());
    /// ```
    ///
    /// The sanctioned factory remains public on the same owner path:
    ///
    /// ```
    /// use silverhand_backend_server::foundation::scope_event_transport::ScopeEventHub;
    ///
    /// let hub = ScopeEventHub::production().unwrap();
    /// let _ = hub.messaging_aftermath((), (), ());
    /// ```
    pub fn messaging_aftermath<D, M, R>(
        &self,
        daemon: D,
        mentions: M,
        unread_read_model: R,
    ) -> MessagingAftermath<HubMessagingEvents, D, M, HubUnreadPublisher<R>> {
        MessagingAftermath::new(
            self.messaging_events(),
            daemon,
            mentions,
            HubUnreadPublisher::new(self.clone(), unread_read_model),
        )
    }

    pub fn thread_follow_aftermath<R>(&self, unread_read_model: R) -> HubThreadFollowUnread<R> {
        HubThreadFollowUnread::new(HubUnreadPublisher::new(self.clone(), unread_read_model))
    }

    /// A narrow unread refresh publisher for effects that mutate read-visible
    /// conversation state outside the ordinary send pipeline (observation,
    /// archive, task and schedule system messages).  Audience and counts remain
    /// owned by the injected read model; the adapter only routes its sealed facts.
    pub fn unread_publisher<R>(&self, unread_read_model: R) -> HubUnreadPublisher<R> {
        HubUnreadPublisher::new(self.clone(), unread_read_model)
    }

    /// Builds the notification-clear aftermath over this hub. Its constructor stays
    /// private so the public seam cannot be rebound to a different transport.
    ///
    /// ```compile_fail
    /// use silverhand_backend_server::{
    ///     conversation_messaging::scope_event_publishers::HubNotificationAftermath,
    ///     foundation::scope_event_transport::ScopeEventHub,
    /// };
    ///
    /// let hub = ScopeEventHub::production().unwrap();
    /// let _ = HubNotificationAftermath::new(hub);
    /// ```
    ///
    /// ```
    /// use silverhand_backend_server::foundation::scope_event_transport::ScopeEventHub;
    ///
    /// let hub = ScopeEventHub::production().unwrap();
    /// let _ = hub.notification_aftermath();
    /// ```
    pub fn notification_aftermath(&self) -> HubNotificationAftermath {
        HubNotificationAftermath::new(self.clone())
    }

    pub fn direct_pairs_changed_sink(&self) -> HubDirectPairsChanged {
        HubDirectPairsChanged::new(self.clone())
    }

    pub fn workspace_rename_publisher(&self) -> HubWorkspaceRenamePublisher {
        HubWorkspaceRenamePublisher(self.clone())
    }

    pub fn saved_publisher(&self) -> HubSavedPublisher {
        HubSavedPublisher(self.clone())
    }

    pub fn attachment_publisher(&self) -> HubAttachmentPublisher {
        HubAttachmentPublisher(self.clone())
    }

    pub fn reaction_publisher(&self) -> HubReactionPublisher {
        HubReactionPublisher(self.clone())
    }

    /// Product-owned invalidations used by the converged conversation, membership,
    /// task, and schedule lanes.  These methods deliberately keep `ProductEvent`
    /// crate-private: composition can name each effect it owes, but cannot publish an
    /// arbitrary event or choose an untyped scope.
    pub fn publish_workspace_channels_changed(
        &self,
        workspace: &WorkspaceId,
        conversation: &ConversationId,
    ) -> Result<(), ScopeEventHubError> {
        self.publish(
            EventScope::Workspace(workspace.clone()),
            ProductEvent::WorkspaceChannelsChanged {
                workspace_id: workspace.as_str().to_owned(),
                channel_id: conversation.as_str().to_owned(),
            },
        )
    }

    pub fn publish_conversation_members_changed(
        &self,
        conversation: &ConversationId,
        workspace: &WorkspaceId,
    ) -> Result<(), ScopeEventHubError> {
        self.publish(
            EventScope::ConversationSide(conversation.clone()),
            ProductEvent::ConversationMembersChanged {
                conversation_id: conversation.as_str().to_owned(),
                workspace_id: workspace.as_str().to_owned(),
            },
        )
    }

    pub fn publish_attention_preference_changed(
        &self,
        viewer: &UserId,
        workspace: &WorkspaceId,
        conversation: Option<&ConversationId>,
    ) -> Result<(), ScopeEventHubError> {
        self.publish(
            EventScope::Viewer(viewer.clone()),
            ProductEvent::AttentionPrefChanged {
                workspace_id: workspace.as_str().to_owned(),
                conversation_id: conversation.map(|id| id.as_str().to_owned()),
            },
        )
    }

    pub fn publish_attention_badge(
        &self,
        viewer: &UserId,
        workspace: &WorkspaceId,
        badge_count: i64,
    ) -> Result<(), ScopeEventHubError> {
        self.publish(
            EventScope::Viewer(viewer.clone()),
            ProductEvent::AttentionBadgeUpdated(AttentionBadge {
                workspace_id: workspace.as_str().to_owned(),
                badge_count,
            }),
        )
    }

    pub fn publish_notification_dismissals(
        &self,
        viewer: &UserId,
        dismissed_notification_ids: Vec<String>,
    ) -> Result<(), ScopeEventHubError> {
        self.publish(
            EventScope::Viewer(viewer.clone()),
            ProductEvent::NotificationDismissed {
                dismissed_notification_ids,
            },
        )
    }

    pub fn publish_task_changed_to_conversation(
        &self,
        conversation: &ConversationId,
        payload: serde_json::Value,
    ) -> Result<(), ScopeEventHubError> {
        self.publish(
            EventScope::Conversation(conversation.clone()),
            ProductEvent::TaskChanged(payload),
        )
    }

    pub fn publish_task_changed_to_viewer(
        &self,
        viewer: &UserId,
        payload: serde_json::Value,
    ) -> Result<(), ScopeEventHubError> {
        self.publish(
            EventScope::Viewer(viewer.clone()),
            ProductEvent::TaskChanged(payload),
        )
    }

    pub fn publish_system_message(
        &self,
        conversation: &ConversationId,
        message: serde_json::Value,
    ) -> Result<(), ScopeEventHubError> {
        self.publish(
            EventScope::Conversation(conversation.clone()),
            ProductEvent::Message(json!({ "message": message })),
        )
    }

    pub fn publish_viewer_workspace_membership_changed(
        &self,
        viewer: &UserId,
        workspace: &WorkspaceId,
        change: &str,
        role: Option<&str>,
    ) -> Result<(), ScopeEventHubError> {
        use crate::foundation::scope_event_wire::ViewerWorkspaceChange;
        let change = match change {
            "added" => ViewerWorkspaceChange::Added,
            "role_changed" => ViewerWorkspaceChange::RoleChanged,
            "removed" => ViewerWorkspaceChange::Removed,
            _ => return Err(ScopeEventHubError::RegistryPoisoned),
        };
        let viewer_event = self.publish(
            EventScope::Viewer(viewer.clone()),
            ProductEvent::ViewerWorkspacesChanged {
                change,
                workspace_id: workspace.as_str().to_owned(),
                role: role.map(str::to_owned),
            },
        );
        let members = self.publish_workspace_members_changed(workspace);
        viewer_event.and(members)
    }

    pub fn publish_direct_pairs_changed(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<(), ScopeEventHubError> {
        self.publish(
            EventScope::Viewer(viewer.clone()),
            ProductEvent::DirectPairsChanged {
                conversation_id: conversation.as_str().to_owned(),
            },
        )
    }

    pub(crate) fn publish_saved_changed(
        &self,
        fact: &SavedChangeFact,
    ) -> Result<(), ScopeEventHubError> {
        self.publish(
            EventScope::Viewer(fact.viewer().clone()),
            ProductEvent::SavedChanged(saved_changed_payload(fact)),
        )
    }

    pub(crate) fn publish_unread_updated_to_viewer(
        &self,
        workspace: &WorkspaceId,
        conversation: &ConversationId,
        viewer: &ViewerUnread,
    ) -> Result<(), ScopeEventHubError> {
        self.publish_batch(
            EventScope::Viewer(viewer.viewer.clone()),
            vec![
                ProductEvent::UnreadUpdated(UnreadUpdated {
                    conversation_id: conversation.as_str().to_owned(),
                    workspace_id: workspace.as_str().to_owned(),
                    conversation_unread_count: viewer.conversation_unread_count,
                    workspace_unread_count: viewer.workspace_unread_count,
                    latest_unread: viewer.latest_unread.as_ref().map(latest_unread_wire),
                }),
                ProductEvent::AttentionBadgeUpdated(AttentionBadge {
                    workspace_id: workspace.as_str().to_owned(),
                    badge_count: viewer.badge_count,
                }),
            ],
        )
    }

    pub(crate) fn publish_notification_unread_cleared_to_viewer(
        &self,
        viewer: &UserId,
        marked: u64,
    ) -> Result<(), ScopeEventHubError> {
        self.publish(
            EventScope::Viewer(viewer.clone()),
            ProductEvent::NotificationUnreadCleared { marked },
        )
    }
}

impl DirectPairOpenedPort for HubMessagingEvents {
    type Error = ScopeEventHubError;

    async fn direct_pair_opened(&self, fact: &DirectPairOpenedFact) -> Result<(), Self::Error> {
        for participant in fact.participants().both() {
            let PrincipalIdentity::Human(viewer) = participant else {
                continue;
            };
            self.publish_direct_pairs_changed(viewer, fact.conversation())?;
        }
        Ok(())
    }
}

impl DirectLineInvalidation for HubMessagingEvents {
    async fn direct_lines_changed(
        &self,
        viewer: &UserId,
        line: &ConversationId,
    ) -> Result<(), EffectFailure> {
        self.publish_direct_pairs_changed(viewer, line)
            .map_err(EffectFailure::from_source)
    }
}

impl MessagePublication for HubMessagingEvents {
    async fn publish_copy(&self, copy: &CommittedMentionCopy) -> Result<(), EffectFailure> {
        // A mention-CC copy IS a message on the direct line it landed on: it rides the
        // same `message` frame every reader of that conversation receives, projected by
        // the one committed-message authority. A copy is never a thread reply, so the
        // payload is `{message}` alone — no `projections` sidecar.
        self.publish(
            EventScope::Conversation(copy.conversation_id.clone()),
            ProductEvent::Message(json!({ "message": copy_dto(copy) })),
        )
        .map_err(EffectFailure::from_source)
    }
}

impl CommittedMessagePublication for HubMessagingEvents {
    async fn publish_message(
        &self,
        committed: CommittedMessageView<'_>,
    ) -> Result<(), EffectFailure> {
        // The one published frame, to the conversation the message landed in — the SAME
        // `{message, projections?}` value the sender's ACK returns. One projector
        // authority, two audiences; `projections` rides only when this is a reply.
        self.publish(
            EventScope::Conversation(committed.message().conversation_id.clone()),
            ProductEvent::Message(message_event_payload(
                committed.message(),
                committed.reply_projection(),
            )),
        )
        .map_err(EffectFailure::from_source)
    }
}

impl WorkspaceRenamePublicationPort for HubWorkspaceRenamePublisher {
    type Error = EffectFailure;

    async fn publish(&self, message: &BoundWorkspaceRenameMessage) -> Result<(), Self::Error> {
        // A committed rename IS a system message on the channel it landed on: it rides
        // the SAME conversation-scope `message` frame every reader receives, projected
        // by the one wire authority. It is never a thread reply, so the payload is
        // `{message}` alone — no `projections` sidecar — exactly like a mention-CC copy.
        //
        // The scope is the SEALED message's own conversation, never a caller
        // coordinate: the frame's scope and the `conversation_id` inside its body are
        // one and the same identity (`rename_dto` reads `draft.conversation()` too),
        // so a message sealed for c1 can never surface on c2's scope.
        self.0
            .publish(
                EventScope::Conversation(message.draft().conversation().clone()),
                ProductEvent::Message(json!({ "message": rename_dto(message) })),
            )
            .map_err(EffectFailure::from_source)
    }
}

impl CopyNotification for HubMessagingEvents {
    async fn notify_author(
        &self,
        author: &UserId,
        _agent: &AgentId,
        copy: &CommittedMentionCopy,
    ) -> Result<(), EffectFailure> {
        // The author's own notice that their mention left the room, addressed to the
        // AUTHOR's viewer scope. Its coordinates are the SOURCE message the author wrote
        // — the room they can still open — not the copy on a line they may not watch.
        let meta = &copy.metadata;
        self.publish(
            EventScope::Viewer(author.clone()),
            ProductEvent::Notification(NotificationEvent {
                kind: NotificationKind::MentionExternal,
                conversation_id: Some(meta.source_conversation_id.as_str().to_owned()),
                message_id: Some(meta.source_message_id.as_str().to_owned()),
            }),
        )
        .map_err(EffectFailure::from_source)
    }
}

impl ViewerNotification for HubMessagingEvents {
    async fn notify_viewer(&self, mention: &CommittedMentionRow) -> Result<(), EffectFailure> {
        // The named reader's own notice, addressed to THEIR viewer scope. Its
        // coordinates are the mentioning message itself, so the client jumps to where it
        // was named. Thin by contract: the excerpt and author stay in the durable row.
        let intent = mention.intent();
        self.publish(
            EventScope::Viewer(mention.viewer().clone()),
            ProductEvent::Notification(NotificationEvent {
                kind: NotificationKind::MentionSelf,
                conversation_id: Some(intent.source_conversation().as_str().to_owned()),
                message_id: Some(intent.source_message().as_str().to_owned()),
            }),
        )
        .map_err(EffectFailure::from_source)
    }
}

/// Saved-message post-commit publication. The fact seals viewer, message and action;
/// this adapter reuses the saved context's sole projector and maps only the transport
/// failure into the capability-owned failure type.
impl SavedAftermath for HubSavedPublisher {
    async fn announce(&self, fact: &SavedChangeFact) -> Result<(), SavedPublicationFailure> {
        self.0
            .publish_saved_changed(fact)
            .map_err(SavedPublicationFailure::from_source)
    }
}

/// The attachment context's cursor-inert post-commit invalidation over the real
/// product transport. The committed fact owns both routing authority and payload
/// authority: this adapter reads its sealed conversation and reuses the attachment
/// context's one wire projector. Keeping the hub error as the associated error leaves
/// the concrete transport failure visible to the route's best-effort logger.
impl AttachmentChangedPublisher for HubAttachmentPublisher {
    type Error = ScopeEventHubError;

    async fn publish(&self, fact: &AttachmentChangedFact) -> Result<(), Self::Error> {
        self.0.publish(
            EventScope::ConversationSide(fact.conversation().clone()),
            ProductEvent::AttachmentChanged(attachment_changed_payload(fact)),
        )
    }
}

/// The reaction context's post-commit event over the real product transport. Unlike
/// attachment byte changes this is part of the canonical message stream, so it rides
/// `Conversation` (not `ConversationSide`) and is consumed by the conversation ingestor.
impl ReactionAftermath for HubReactionPublisher {
    async fn announce(&self, fact: &ReactionChangedFact) -> Result<(), ReactionPublicationFailure> {
        let payload = serde_json::to_value(reaction_changed_payload(fact))
            .map_err(ReactionPublicationFailure::from_source)?;
        self.0
            .publish(
                EventScope::Conversation(fact.conversation().clone()),
                ProductEvent::MessageReactionChanged(payload),
            )
            .map_err(ReactionPublicationFailure::from_source)
    }
}

/// The wire projection of one unread anchor. `None` upstream stays `None`; a present
/// anchor is spelled out coordinate by coordinate, never a stale carry-over.
pub(crate) fn latest_unread_wire(latest: &CoreLatestUnread) -> LatestUnread {
    LatestUnread {
        conversation_id: latest.conversation.as_str().to_owned(),
        message_id: latest.message.as_str().to_owned(),
        seq: latest.seq,
        thread_root_id: latest
            .thread_root
            .as_ref()
            .map(|root| root.as_str().to_owned()),
    }
}

/// Composes the hub with a read model into the production [`UnreadPublication`] effect.
///
/// The read model is a TYPE PARAMETER, not a concrete adapter: backend-server depends on
/// the SQLite crate only in dev, so the product read model is injected at the composition
/// root and no engine type appears here. This publisher owns only the routing — the read
/// model decides placement, audience, and every count — so the placement fork below is a
/// total match over [`UnreadAudience`]'s two arms and cannot silently drop one.
///
/// Outside the crate it comes only from [`ScopeEventHub::messaging_aftermath`], which
/// supplies the hub from its own `&self`; the constructor stays crate-private, and in-crate
/// tests call it directly as substrate. It needs no `Clone`: the aftermath that owns it is
/// an `Arc` handle, so the second door shares this publisher rather than duplicating it.
pub struct HubUnreadPublisher<R> {
    hub: ScopeEventHub,
    read_model: R,
}

impl<R> HubUnreadPublisher<R> {
    pub(crate) fn new(hub: ScopeEventHub, read_model: R) -> Self {
        Self { hub, read_model }
    }
}

impl<R: UnreadReadModel + Send + Sync> UnreadPublication for HubUnreadPublisher<R> {
    async fn publish_unread(&self, conversation: &ConversationId) -> Result<(), EffectFailure> {
        // Who is owed an update, and their state, is decided against the committed
        // database by the read model; this ROUTES the answer and re-decides nothing.
        // `None` is the conversation gone — nothing to publish.
        let Some(audience) = self
            .read_model
            .unread_audience(conversation)
            .await
            .map_err(EffectFailure::from_source)?
        else {
            return Ok(());
        };

        match audience {
            // Workspace placement: each seated viewer receives the unread+badge co-emit,
            // through the one door that emits both. A scope-local transport failure is
            // isolated to that viewer: retain the first cause, but still attempt every
            // already-computed audience before returning it to the outer effect report.
            UnreadAudience::Workspace { workspace, viewers } => {
                let mut first_failure = None;
                for viewer in &viewers {
                    if let Err(cause) =
                        self.hub
                            .publish_unread_updated_to_viewer(&workspace, conversation, viewer)
                        && first_failure.is_none()
                    {
                        first_failure = Some(EffectFailure::from_source(cause));
                    }
                }
                if let Some(failure) = first_failure {
                    return Err(failure);
                }
            }
            // A direct line has no workspace unread surface: each human endpoint's
            // direct-line LIST is invalidated instead — the distinct DM axis, the same
            // frame arrival and clearing both ride. The same per-viewer isolation rule
            // applies here.
            UnreadAudience::DirectPair { viewers } => {
                let mut first_failure = None;
                for viewer in &viewers {
                    if let Err(cause) = self.hub.publish_direct_pairs_changed(viewer, conversation)
                        && first_failure.is_none()
                    {
                        first_failure = Some(EffectFailure::from_source(cause));
                    }
                }
                if let Some(failure) = first_failure {
                    return Err(failure);
                }
            }
        }
        Ok(())
    }
}

impl<R: ConversationHumanAudience + Send + Sync> LatestPublication for HubUnreadPublisher<R> {
    async fn publish_latest(&self, latest: &ConversationLatest) -> Result<(), EffectFailure> {
        let viewers = self
            .read_model
            .human_participants(&latest.conversation)
            .await
            .map_err(EffectFailure::from_source)?;
        let payload = latest_dto(latest);
        let mut first_failure = None;
        for viewer in viewers {
            if let Err(cause) = self.hub.publish(
                EventScope::Viewer(viewer),
                ProductEvent::ConversationLatestChanged(payload.clone()),
            ) && first_failure.is_none()
            {
                first_failure = Some(EffectFailure::from_source(cause));
            }
        }
        first_failure.map_or(Ok(()), Err)
    }
}

impl<R: MessageAlertAudience + Send + Sync> AlertPublication for HubUnreadPublisher<R> {
    async fn publish_alerts(
        &self,
        message: &CreatedMessage,
        named: &[ParticipantHandle],
    ) -> Result<(), EffectFailure> {
        let Some(audience) = self
            .read_model
            .alert_audience(&message.conversation_id, message)
            .await
            .map_err(EffectFailure::from_source)?
        else {
            return Ok(());
        };
        let mut first_failure = None;
        for recipient in &audience.recipients {
            let Some(reason) = alert_reason(message, named, recipient) else {
                continue;
            };
            if let Err(cause) = self.hub.publish(
                EventScope::Viewer(recipient.viewer.clone()),
                ProductEvent::MessageAlert(message_alert_payload(message, &audience.place, reason)),
            ) && first_failure.is_none()
            {
                first_failure = Some(EffectFailure::from_source(cause));
            }
        }
        first_failure.map_or(Ok(()), Err)
    }
}

/// One reader's alert: where the message is, who sent it, what it says and why it
/// interrupts them.
fn message_alert_payload(
    message: &CreatedMessage,
    place: &AlertPlace,
    reason: AlertReason,
) -> serde_json::Value {
    json!({
        "conversation_id": message.conversation_id.as_str(),
        "workspace_id": place.workspace.as_ref().map(|id| id.as_str()),
        "conversation_title": place.channel_title,
        "message_id": message.id.as_str(),
        "thread_root_id": message.thread_root_id.as_ref().map(|id| id.as_str()),
        "reason": reason.as_str(),
        "sender": message.author.as_ref().map(author_dto),
        "excerpt": mention_excerpt(message.content.as_str()),
    })
}

/// Notification-clear post-commit effect. It emits only the acting viewer's thin
/// `unread_updated { marked }` refetch signal; it never publishes an absolute badge
/// value that could regress under out-of-order delivery. The remaining cross-producer
/// badge ordering question stays explicit outside this adapter.
#[derive(Clone)]
pub struct HubNotificationAftermath {
    hub: ScopeEventHub,
}

impl HubNotificationAftermath {
    pub(crate) fn new(hub: ScopeEventHub) -> Self {
        Self { hub }
    }
}

impl NotificationAftermath for HubNotificationAftermath {
    async fn unread_changed(
        &self,
        fact: &CommittedNotificationClear,
    ) -> Result<(), NotificationClearFailure> {
        self.hub
            .publish_notification_unread_cleared_to_viewer(fact.viewer(), fact.marked())
            .map_err(NotificationClearFailure::from_source)
    }
}

/// Agent-creation direct-pair invalidation. The public hub factory is the only way
/// to mint this adapter, and the constructor stays crate-private.
#[derive(Clone)]
pub struct HubDirectPairsChanged {
    hub: ScopeEventHub,
}

impl HubDirectPairsChanged {
    pub(crate) fn new(hub: ScopeEventHub) -> Self {
        Self { hub }
    }
}

impl crate::application_orchestration::agent_create::DirectPairsChanged for HubDirectPairsChanged {
    async fn direct_pairs_changed(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
    ) -> Result<(), EffectFailure> {
        self.hub
            .publish_direct_pairs_changed(viewer, conversation)
            .map_err(EffectFailure::from_source)
    }
}

/// Thread-follow aftermath composed with the same unread publisher used by message
/// aftermath. The committed read model recomputes the full audience and counts; this
/// adapter does not trust or project a stale counter from the follow fact.
pub struct HubThreadFollowUnread<R> {
    publisher: HubUnreadPublisher<R>,
}

impl<R> HubThreadFollowUnread<R> {
    pub(crate) fn new(publisher: HubUnreadPublisher<R>) -> Self {
        Self { publisher }
    }
}

impl<R: UnreadReadModel + Send + Sync> ThreadFollowAftermath for HubThreadFollowUnread<R> {
    async fn unread_changed(
        &self,
        fact: ThreadFollowChangedFact<'_>,
    ) -> Result<(), ThreadFollowPublicationFailure> {
        // The follow changed which replies this conversation owes its viewers, so the
        // conversation's unread is republished through the one publisher. The viewer on
        // the fact is already part of that audience — recomputed against the committed
        // record, never trusted from here.
        self.publisher
            .publish_unread(fact.conversation())
            .await
            .map_err(ThreadFollowPublicationFailure::from_source)
    }
}
