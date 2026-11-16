//! Closed product-event vocabulary carried by the scope transport.

use serde::Serialize;
use serde_json::Value;

/// A product event, tagged so a client discriminates on `kind` and reads `payload`.
///
/// `Value` variants are already-projected, owned payload seals. Their bounded context
/// remains the sole wire-shape authority; the transport must not reproduce those schemas.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub(crate) enum ProductEvent {
    SidebarSortChanged {},
    DirectPairsChanged {
        conversation_id: String,
    },
    HostConnectionChanged {
        host_id: String,
    },
    AgentActivity(Value),
    AgentStatus(Value),
    AgentInitTimeout(Value),
    CompactionStale(Value),
    AgentEvent(Value),
    ConversationActivity(Value),
    AgentStreamActivity(Value),
    AgentPresenceChanged {
        agent_id: String,
    },
    AgentAvailabilityChanged {
        agent_id: String,
    },
    AgentChanged {
        agent_id: String,
    },
    UserChanged {
        user_id: String,
    },
    UserPresence {
        user_id: String,
        presence: &'static str,
        last_seen_at: Option<i64>,
    },
    Message(Value),
    WorkspaceMembersChanged {
        workspace_id: String,
    },
    WorkspaceChannelsChanged {
        workspace_id: String,
        channel_id: String,
    },
    ConversationMembersChanged {
        conversation_id: String,
        workspace_id: String,
    },
    AttentionPrefChanged {
        workspace_id: String,
        conversation_id: Option<String>,
    },
    TaskChanged(Value),
    Notification(NotificationEvent),
    UnreadUpdated(UnreadUpdated),
    #[serde(rename = "unread_updated")]
    NotificationUnreadCleared {
        marked: u64,
    },
    AttentionBadgeUpdated(AttentionBadge),
    #[serde(rename = "unread_updated")]
    NotificationDismissed {
        dismissed_notification_ids: Vec<String>,
    },
    ViewerWorkspacesChanged {
        change: ViewerWorkspaceChange,
        workspace_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        role: Option<String>,
    },
    SavedChanged(Value),
    MessageReactionChanged(Value),
    AttachmentChanged(Value),
    ConversationLatestChanged(Value),
    MessageAlert(Value),
    #[cfg(test)]
    Test {
        value: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ViewerWorkspaceChange {
    Added,
    RoleChanged,
    Removed,
    MetadataChanged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NotificationKind {
    MentionSelf,
    MentionExternal,
    AgentStartFailed,
}

/// `agent_start_failed` has no message anchor and may have no conversation the
/// owner can read, so both coordinates are optional on the wire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct NotificationEvent {
    pub(crate) kind: NotificationKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) conversation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) message_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct UnreadUpdated {
    pub(crate) conversation_id: String,
    pub(crate) workspace_id: String,
    pub(crate) conversation_unread_count: i64,
    pub(crate) workspace_unread_count: i64,
    pub(crate) latest_unread: Option<LatestUnread>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct LatestUnread {
    pub(crate) conversation_id: String,
    pub(crate) message_id: String,
    pub(crate) seq: i64,
    pub(crate) thread_root_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct AttentionBadge {
    pub(crate) workspace_id: String,
    pub(crate) badge_count: i64,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{ProductEvent, ViewerWorkspaceChange};

    #[test]
    fn typed_contracts_and_owned_projection_seals_keep_their_exact_wire_shapes() {
        let typed = ProductEvent::ViewerWorkspacesChanged {
            change: ViewerWorkspaceChange::Added,
            workspace_id: "workspace-1".to_owned(),
            role: Some("member".to_owned()),
        };
        assert_eq!(
            serde_json::to_value(typed).unwrap(),
            json!({
                "kind": "viewer_workspaces_changed",
                "payload": {
                    "change": "added",
                    "workspace_id": "workspace-1",
                    "role": "member"
                }
            })
        );

        let sealed = json!({
            "message": {"id": "message-1", "nested": {"future_field": true}},
            "projections": [{"type": "mention", "opaque": 7}]
        });
        assert_eq!(
            serde_json::to_value(ProductEvent::Message(sealed.clone())).unwrap(),
            json!({"kind": "message", "payload": sealed})
        );
    }
}
