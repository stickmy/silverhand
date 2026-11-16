//! The viewer's Activity cold projection.
//!
//! Activity owns no durable state. It lists one row per stream (main line or thread)
//! that waits on the viewer — unread messages it ambiently enters, or an unread
//! mention of the viewer — plus agent start failures. This module owns the closed
//! projected values and the consumer-language read port; SQL and HTTP remain in
//! their adapter crates.

use std::{cmp::Ordering, error::Error, future::Future};

use crate::{
    conversation_messaging::direct_pair_read::DirectPairCounterpart,
    conversation_messaging::messaging::{MessageAuthor, MessageId, NotificationId, ThreadRootId},
    foundation::identity::{ConversationId, UserId, WorkspaceId},
};

pub mod observe;

const PREVIEW_MAX_UTF16: usize = 140;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ActivitySalience {
    Directed,
    Ambient,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActivityLocation {
    WorkspaceChannel {
        workspace: WorkspaceId,
        title: String,
    },
    DirectPair {
        counterpart: DirectPairCounterpart,
    },
}

impl ActivityLocation {
    pub fn workspace_channel(workspace: WorkspaceId, title: String) -> Self {
        Self::WorkspaceChannel { workspace, title }
    }

    pub fn direct_pair(counterpart: DirectPairCounterpart) -> Self {
        Self::DirectPair { counterpart }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("an Activity unread count cannot be negative")]
pub struct NegativeActivityUnreadCount;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActivityUnreadCount(i64);

impl ActivityUnreadCount {
    pub fn new(value: i64) -> Result<Self, NegativeActivityUnreadCount> {
        if value < 0 {
            Err(NegativeActivityUnreadCount)
        } else {
            Ok(Self(value))
        }
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

/// A display excerpt after the production ECMAScript `\s+`, `trim`, and
/// UTF-16-unit ceiling. `None` is represented by the outer option: no empty
/// preview can enter the projected value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityPreview(String);

impl ActivityPreview {
    pub fn from_source(raw: Option<&str>) -> Option<Self> {
        let raw = raw.filter(|value| !value.is_empty())?;
        let mut compact = String::with_capacity(raw.len());
        let mut pending_space = false;
        for scalar in raw.chars() {
            if is_ecmascript_space(scalar) {
                if !compact.is_empty() {
                    pending_space = true;
                }
            } else {
                if pending_space {
                    compact.push(' ');
                    pending_space = false;
                }
                compact.push(scalar);
            }
        }
        if compact.is_empty() {
            return None;
        }
        if compact.encode_utf16().count() <= PREVIEW_MAX_UTF16 {
            return Some(Self(compact));
        }

        // The retired TS implementation used `slice(0, 139)`, which can cut an
        // astral scalar between its surrogate halves. The Rust cutover
        // deliberately does not preserve that malformed string: retain the
        // longest complete-scalar prefix not exceeding 139 units, then append
        // the ellipsis. The product decision keeps the UTF-16 budget while
        // requiring every emitted preview to remain valid Unicode.
        let mut units = 0_usize;
        let mut boundary = 0_usize;
        for (offset, scalar) in compact.char_indices() {
            let next = units + scalar.len_utf16();
            if next > PREVIEW_MAX_UTF16 - 1 {
                break;
            }
            units = next;
            boundary = offset + scalar.len_utf8();
        }
        compact.truncate(boundary);
        compact.push('…');
        Some(Self(compact))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn is_ecmascript_space(value: char) -> bool {
    matches!(
        value,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActivitySource {
    AgentFailed {
        notification: NotificationId,
    },
    /// A stream waiting on the viewer, anchored at its first unread mention, else its
    /// first unread message. `unread` counts every unread message in it; `mentions`
    /// the unread ones that name the viewer.
    Stream {
        message: MessageId,
        unread: ActivityUnreadCount,
        mentions: ActivityUnreadCount,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityItem {
    source: ActivitySource,
    salience: ActivitySalience,
    conversation: ConversationId,
    thread_root: Option<ThreadRootId>,
    location: ActivityLocation,
    thread_root_preview: Option<ActivityPreview>,
    preview: Option<ActivityPreview>,
    preview_sender: Option<MessageAuthor>,
    timestamp_ms: i64,
}

impl ActivityItem {
    pub fn agent_failed(
        notification: NotificationId,
        conversation: ConversationId,
        location: ActivityLocation,
        preview: Option<ActivityPreview>,
        timestamp_ms: i64,
    ) -> Self {
        Self {
            source: ActivitySource::AgentFailed { notification },
            salience: ActivitySalience::Directed,
            conversation,
            thread_root: None,
            location,
            thread_root_preview: None,
            preview,
            preview_sender: None,
            timestamp_ms,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn stream(
        message: MessageId,
        unread: ActivityUnreadCount,
        mentions: ActivityUnreadCount,
        salience: ActivitySalience,
        conversation: ConversationId,
        thread_root: Option<ThreadRootId>,
        location: ActivityLocation,
        thread_root_preview: Option<ActivityPreview>,
        preview: Option<ActivityPreview>,
        preview_sender: Option<MessageAuthor>,
        timestamp_ms: i64,
    ) -> Self {
        Self {
            source: ActivitySource::Stream {
                message,
                unread,
                mentions,
            },
            salience,
            conversation,
            thread_root,
            location,
            thread_root_preview,
            preview,
            preview_sender,
            timestamp_ms,
        }
    }

    pub fn source(&self) -> &ActivitySource {
        &self.source
    }
    pub fn salience(&self) -> ActivitySalience {
        self.salience
    }
    pub fn conversation(&self) -> &ConversationId {
        &self.conversation
    }
    pub fn thread_root(&self) -> Option<&ThreadRootId> {
        self.thread_root.as_ref()
    }
    pub fn location(&self) -> &ActivityLocation {
        &self.location
    }
    pub fn thread_root_preview(&self) -> Option<&ActivityPreview> {
        self.thread_root_preview.as_ref()
    }
    pub fn preview(&self) -> Option<&ActivityPreview> {
        self.preview.as_ref()
    }
    pub fn preview_sender(&self) -> Option<&MessageAuthor> {
        self.preview_sender.as_ref()
    }
    pub fn timestamp_ms(&self) -> i64 {
        self.timestamp_ms
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityList {
    items: Vec<ActivityItem>,
}

impl ActivityList {
    /// Timestamp ties close on persistent coordinates: agent failures before streams,
    /// then by identity, never by rowid or query-plan order.
    pub fn projected(mut items: Vec<ActivityItem>) -> Self {
        items.sort_by(|left, right| {
            left.salience()
                .cmp(&right.salience())
                .then_with(|| right.timestamp_ms().cmp(&left.timestamp_ms()))
                .then_with(|| source_coordinate(left, right))
        });
        Self { items }
    }

    pub fn items(&self) -> &[ActivityItem] {
        &self.items
    }
}

fn source_coordinate(left: &ActivityItem, right: &ActivityItem) -> Ordering {
    match (left.source(), right.source()) {
        (
            ActivitySource::AgentFailed { notification: left },
            ActivitySource::AgentFailed {
                notification: right,
            },
        ) => left.as_str().cmp(right.as_str()),
        (ActivitySource::Stream { .. }, ActivitySource::Stream { .. }) => left
            .conversation()
            .as_str()
            .cmp(right.conversation().as_str())
            .then_with(|| {
                left.thread_root()
                    .map(ThreadRootId::as_str)
                    .cmp(&right.thread_root().map(ThreadRootId::as_str))
            }),
        (ActivitySource::AgentFailed { .. }, ActivitySource::Stream { .. }) => Ordering::Less,
        (ActivitySource::Stream { .. }, ActivitySource::AgentFailed { .. }) => Ordering::Greater,
    }
}

pub trait ActivityReadModel {
    type Error: Error + Send + Sync + 'static;

    fn list_activity(
        &self,
        viewer: &UserId,
        workspace: Option<&WorkspaceId>,
    ) -> impl Future<Output = Result<ActivityList, Self::Error>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_uses_ecmascript_space_and_utf16_budget() {
        let preview =
            ActivityPreview::from_source(Some("\u{FEFF}hello\u{0085}  world\u{FEFF}")).unwrap();
        assert_eq!(preview.as_str(), "hello\u{0085} world");
        assert!(ActivityPreview::from_source(Some(" \n\u{FEFF} ")).is_none());

        let exact = "😀".repeat(70);
        assert_eq!(
            ActivityPreview::from_source(Some(&exact)).unwrap().as_str(),
            exact
        );
        let over = format!("{}x", "😀".repeat(70));
        let compact = ActivityPreview::from_source(Some(&over)).unwrap();
        assert_eq!(compact.as_str(), format!("{}…", "😀".repeat(69)));
        assert_eq!(compact.as_str().encode_utf16().count(), 139);
    }

    fn stream(conversation: &str, salience: ActivitySalience, timestamp_ms: i64) -> ActivityItem {
        ActivityItem::stream(
            MessageId::parse(format!("m-{conversation}")).unwrap(),
            ActivityUnreadCount::new(1).unwrap(),
            ActivityUnreadCount::new(0).unwrap(),
            salience,
            ConversationId::parse(conversation).unwrap(),
            None,
            ActivityLocation::workspace_channel(
                WorkspaceId::parse("w-1").unwrap(),
                "General".to_owned(),
            ),
            None,
            None,
            None,
            timestamp_ms,
        )
    }

    #[test]
    fn persistent_coordinates_total_order_timestamp_ties() {
        let failed = ActivityItem::agent_failed(
            NotificationId::parse("n-1").unwrap(),
            ConversationId::parse("c-z").unwrap(),
            ActivityLocation::workspace_channel(
                WorkspaceId::parse("w-1").unwrap(),
                "General".to_owned(),
            ),
            None,
            10,
        );
        let z = stream("c-z", ActivitySalience::Directed, 10);
        let a = stream("c-a", ActivitySalience::Directed, 10);
        let ambient = stream("c-0", ActivitySalience::Ambient, 20);

        let expected = vec![failed.clone(), a.clone(), z.clone(), ambient.clone()];
        assert_eq!(
            ActivityList::projected(vec![z.clone(), ambient.clone(), failed.clone(), a.clone()])
                .items(),
            expected
        );
        assert_eq!(
            ActivityList::projected(vec![ambient, a, z, failed]).items(),
            expected
        );
    }
}
