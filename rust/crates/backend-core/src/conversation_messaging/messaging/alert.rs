//! Whether a committed message should interrupt each person who can read it.
//!
//! An alert is a moment, not a record: nothing here is stored. Unread state and
//! mention records keep their own authorities; this only decides, per message and
//! per human recipient, whether to notify now and why.

use std::future::Future;

use super::{CreatedMessage, FromKind, ParticipantHandle};
use crate::foundation::{
    identity::{ConversationId, UserId, WorkspaceId},
    persistence::PersistenceFailure,
};

/// Why a message alerts its reader. Ordered by precedence: a message that is both a
/// direct line and names its reader alerts once, as `Direct`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AlertReason {
    /// A conversation with a fixed roster: a direct pair, or a workspace line such as
    /// an agent's home channel.
    Direct,
    /// The body named the reader. Not silenced by channel or workspace mute.
    Mention,
    /// The reader's channel level is "all messages" and the message is in a stream
    /// they watch: the main line, or a thread they follow.
    Ambient,
}

impl AlertReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Mention => "mention",
            Self::Ambient => "ambient",
        }
    }
}

/// What the decision needs to know about one live human participant, read against the
/// committed database.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AlertRecipient {
    pub viewer: UserId,
    pub handle: ParticipantHandle,
    pub standing: AlertStanding,
}

/// How the reader relates to the conversation the message landed in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AlertStanding {
    Direct,
    Channel {
        /// The channel level is "all messages" and the workspace is not muted.
        ambient: bool,
        /// The reader follows the message's thread. Meaningless on the main line.
        follows_thread: bool,
    },
}

/// Where an alert points the reader: the conversation's workspace, and its title when
/// it is a named channel (never for a direct line).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AlertPlace {
    pub workspace: Option<WorkspaceId>,
    pub channel_title: Option<String>,
}

/// One message's conversation and its live human participants with their standing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AlertAudience {
    pub place: AlertPlace,
    pub recipients: Vec<AlertRecipient>,
}

/// Reads [`AlertAudience`] for a committed message; `None` when the conversation is gone.
pub trait MessageAlertAudience {
    fn alert_audience(
        &self,
        conversation: &ConversationId,
        message: &CreatedMessage,
    ) -> impl Future<Output = Result<Option<AlertAudience>, PersistenceFailure>> + Send;
}

/// The reason `recipient` is alerted by `message`, or `None`. `named` is the set of
/// human handles the body addressed.
pub fn alert_reason(
    message: &CreatedMessage,
    named: &[ParticipantHandle],
    recipient: &AlertRecipient,
) -> Option<AlertReason> {
    if message.from_kind == FromKind::System
        || message.from_user_id.as_ref() == Some(&recipient.viewer)
    {
        return None;
    }
    match recipient.standing {
        AlertStanding::Direct => Some(AlertReason::Direct),
        _ if named.contains(&recipient.handle) => Some(AlertReason::Mention),
        AlertStanding::Channel {
            ambient: true,
            follows_thread,
        } if message.thread_root_id.is_none() || follows_thread => Some(AlertReason::Ambient),
        AlertStanding::Channel { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_messaging::messaging::{
        MessageContent, MessageId, MessageSeq, ThreadRootId,
    };

    fn message(from_user: Option<&str>, thread: bool) -> CreatedMessage {
        CreatedMessage {
            client_message_id: None,
            id: MessageId::parse("m-1").unwrap(),
            conversation_id: ConversationId::parse("c-1").unwrap(),
            from_kind: if from_user.is_some() {
                FromKind::User
            } else {
                FromKind::Agent
            },
            from_agent_id: None,
            from_user_id: from_user.map(|id| UserId::parse(id).unwrap()),
            author: None,
            content: MessageContent::parse("hello").unwrap(),
            attachments: Vec::new(),
            seq: MessageSeq::try_from(1).unwrap(),
            created_at_ms: 1,
            thread_root_id: thread.then(|| ThreadRootId::parse("root").unwrap()),
            options: None,
        }
    }

    fn recipient(standing: AlertStanding) -> AlertRecipient {
        AlertRecipient {
            viewer: UserId::parse("u-1").unwrap(),
            handle: ParticipantHandle::parse("alice").unwrap(),
            standing,
        }
    }

    fn channel(ambient: bool, follows_thread: bool) -> AlertStanding {
        AlertStanding::Channel {
            ambient,
            follows_thread,
        }
    }

    fn named() -> Vec<ParticipantHandle> {
        vec![ParticipantHandle::parse("alice").unwrap()]
    }

    #[test]
    fn a_direct_line_alerts_every_message_from_the_other_side() {
        let direct = recipient(AlertStanding::Direct);
        assert_eq!(
            alert_reason(&message(None, false), &[], &direct),
            Some(AlertReason::Direct)
        );
        assert_eq!(
            alert_reason(&message(None, true), &named(), &direct),
            Some(AlertReason::Direct)
        );
    }

    #[test]
    fn the_sender_and_system_messages_never_alert() {
        let direct = recipient(AlertStanding::Direct);
        assert_eq!(
            alert_reason(&message(Some("u-1"), false), &named(), &direct),
            None
        );
        let mut system = message(None, false);
        system.from_kind = FromKind::System;
        assert_eq!(alert_reason(&system, &named(), &direct), None);
    }

    #[test]
    fn a_mention_alerts_even_when_the_channel_is_quiet() {
        let quiet = recipient(channel(false, false));
        assert_eq!(
            alert_reason(&message(None, true), &named(), &quiet),
            Some(AlertReason::Mention)
        );
        assert_eq!(alert_reason(&message(None, false), &[], &quiet), None);
    }

    #[test]
    fn all_messages_alerts_the_main_line_and_followed_threads_only() {
        let loud = recipient(channel(true, false));
        assert_eq!(
            alert_reason(&message(None, false), &[], &loud),
            Some(AlertReason::Ambient)
        );
        assert_eq!(alert_reason(&message(None, true), &[], &loud), None);
        let following = recipient(channel(true, true));
        assert_eq!(
            alert_reason(&message(None, true), &[], &following),
            Some(AlertReason::Ambient)
        );
    }
}
