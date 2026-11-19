//! Point-read candidates, authorization, and viewer-resolved detail navigation.

use std::future::Future;

use crate::conversation_messaging::messaging::MessageId;
use crate::foundation::identity::{AgentId, ConversationId, UserId};
use crate::foundation::persistence::PersistenceFailure;

use super::{
    BoardKey, LastEditor, ReadViewer, TaskAggregate, TaskDisplaySidecars, TaskParticipationReads,
    TaskReadAddress, TaskReadCapability, TaskReadRequest, TaskSummarySource, read_visible_task_on,
};

pub trait TaskConversationBoardRead: Send + Sync + 'static {
    fn conversation_board(
        &self,
        conversation: &ConversationId,
    ) -> impl Future<Output = Result<Option<BoardKey>, PersistenceFailure>> + Send;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskMessageRef {
    pub message_id: MessageId,
    pub conversation_id: ConversationId,
    pub thread_root_id: Option<super::ThreadRootId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskEditorDisplay {
    Agent { id: AgentId, name: String },
    User { id: UserId, name: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskNavigation {
    Source(TaskMessageRef),
    CreatedEvent(TaskMessageRef),
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskPointCandidate {
    task: TaskAggregate,
    display: TaskDisplaySidecars,
    editor: Option<TaskEditorDisplay>,
    source_ref: Option<TaskMessageRef>,
    created_event_ref: Option<TaskMessageRef>,
}

impl TaskPointCandidate {
    pub fn new(
        task: TaskAggregate,
        display: TaskDisplaySidecars,
        editor_name: Option<String>,
        source_ref: Option<TaskMessageRef>,
        created_event_ref: Option<TaskMessageRef>,
    ) -> Self {
        let editor = task.last_edit().map(|edit| match &edit.editor {
            LastEditor::Agent(id) => TaskEditorDisplay::Agent {
                id: id.clone(),
                name: editor_name
                    .clone()
                    .unwrap_or_else(|| id.as_str().to_owned()),
            },
            LastEditor::User(id) => TaskEditorDisplay::User {
                id: id.clone(),
                name: editor_name.unwrap_or_else(|| id.as_str().to_owned()),
            },
        });
        Self {
            task,
            display,
            editor,
            source_ref,
            created_event_ref,
        }
    }

    pub fn task(&self) -> &TaskAggregate {
        &self.task
    }
}

pub trait TaskPointFetch {
    fn fetch_point(
        &self,
        address: &TaskReadAddress,
    ) -> impl Future<Output = Result<Option<TaskPointCandidate>, PersistenceFailure>> + Send;
}

/// A complete viewer-authorized point projection produced from one
/// connection-bound read snapshot.
pub trait TaskAuthorizedPointRead {
    fn read_authorized_point(
        &self,
        request: TaskReadRequest,
    ) -> impl Future<Output = Result<Option<VisibleTaskPoint>, PersistenceFailure>> + Send;

    /// Public by-number read whose conversation→board resolution and final
    /// task/detail authorization must share the same database snapshot.
    fn read_authorized_point_by_conversation_number(
        &self,
        viewer: UserId,
        conversation: ConversationId,
        number: super::TaskNumber,
    ) -> impl Future<Output = Result<Option<VisibleTaskPoint>, PersistenceFailure>> + Send;
}

/// One task lifecycle system message; `metadata` is the stored event payload.
#[derive(Clone, Debug, PartialEq)]
pub struct TaskHistoryEvent {
    pub id: MessageId,
    pub created_at: i64,
    pub metadata: serde_json::Value,
}

pub trait TaskHistoryRead {
    /// `None` exactly when the human by-id point read would be `None`; the gate
    /// and the event fetch share one snapshot.
    fn read_authorized_history(
        &self,
        viewer: UserId,
        task: super::TaskId,
    ) -> impl Future<Output = Result<Option<Vec<TaskHistoryEvent>>, PersistenceFailure>> + Send;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VisibleTaskPoint {
    viewer: ReadViewer,
    task: TaskAggregate,
    display: TaskDisplaySidecars,
    editor: Option<TaskEditorDisplay>,
    nav: TaskNavigation,
}

pub struct TaskPointSource<'a> {
    pub summary: TaskSummarySource<'a>,
    pub editor: Option<&'a TaskEditorDisplay>,
    pub nav: &'a TaskNavigation,
}

impl VisibleTaskPoint {
    /// Mint the viewer-bound detail captured by an edit UoW after its in-lock
    /// participation/capability gate. Navigation references must remain in the
    /// task's own conversation; create admission establishes that invariant and
    /// this constructor fails closed if persisted data violates it.
    pub fn from_committed_edit(viewer: ReadViewer, candidate: TaskPointCandidate) -> Option<Self> {
        let TaskPointCandidate {
            task,
            display,
            editor,
            source_ref,
            created_event_ref,
        } = candidate;
        let chosen = if task.source_message().is_some() {
            source_ref.map(TaskNavigation::Source)
        } else {
            created_event_ref.map(TaskNavigation::CreatedEvent)
        };
        let nav = match chosen {
            Some(TaskNavigation::Source(reference))
                if reference.conversation_id == *task.conversation_id() =>
            {
                TaskNavigation::Source(reference)
            }
            Some(TaskNavigation::CreatedEvent(reference))
                if reference.conversation_id == *task.conversation_id() =>
            {
                TaskNavigation::CreatedEvent(reference)
            }
            Some(_) => return None,
            None => TaskNavigation::Unavailable,
        };
        Some(Self {
            viewer,
            task,
            display,
            editor,
            nav,
        })
    }

    pub fn for_viewer(&self, viewer: &ReadViewer) -> Option<TaskPointSource<'_>> {
        (self.viewer == *viewer).then_some(TaskPointSource {
            summary: TaskSummarySource {
                task: &self.task,
                display: &self.display,
            },
            editor: self.editor.as_ref(),
            nav: &self.nav,
        })
    }
}

pub async fn authorize_task_point_on<A>(
    request: &TaskReadRequest,
    candidate: TaskPointCandidate,
    authority: &mut A,
) -> Result<Option<VisibleTaskPoint>, PersistenceFailure>
where
    A: TaskParticipationReads + TaskReadCapability,
{
    let TaskPointCandidate {
        task,
        display,
        editor,
        source_ref,
        created_event_ref,
    } = candidate;
    let Some(visible) = read_visible_task_on(request, task, authority).await? else {
        return Ok(None);
    };
    let (viewer, task) = visible.into_parts();
    let chosen = if task.source_message().is_some() {
        source_ref.map(TaskNavigation::Source)
    } else {
        created_event_ref.map(TaskNavigation::CreatedEvent)
    };
    let nav = match chosen {
        Some(TaskNavigation::Source(reference))
            if can_read_ref(&viewer, &reference, authority).await? =>
        {
            TaskNavigation::Source(reference)
        }
        Some(TaskNavigation::CreatedEvent(reference))
            if can_read_ref(&viewer, &reference, authority).await? =>
        {
            TaskNavigation::CreatedEvent(reference)
        }
        _ => TaskNavigation::Unavailable,
    };
    Ok(Some(VisibleTaskPoint {
        viewer,
        task,
        display,
        editor,
        nav,
    }))
}

async fn can_read_ref<P: TaskParticipationReads>(
    viewer: &ReadViewer,
    reference: &TaskMessageRef,
    participation: &mut P,
) -> Result<bool, PersistenceFailure> {
    match viewer {
        ReadViewer::Agent(agent) => {
            participation
                .is_agent_participant(&reference.conversation_id, agent)
                .await
        }
        ReadViewer::User(user) => {
            participation
                .is_user_participant(&reference.conversation_id, user)
                .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_messaging::messaging::task::{TaskId, TaskReadActor, TaskRow};
    use crate::foundation::identity::WorkspaceId;
    use std::collections::HashSet;

    struct Authority;
    impl TaskParticipationReads for Authority {
        async fn is_agent_participant(
            &mut self,
            _: &ConversationId,
            _: &AgentId,
        ) -> Result<bool, PersistenceFailure> {
            Ok(true)
        }
        async fn is_user_participant(
            &mut self,
            _: &ConversationId,
            _: &UserId,
        ) -> Result<bool, PersistenceFailure> {
            Ok(true)
        }
        async fn agent_participating_conversations(
            &mut self,
            _: &AgentId,
            values: &[ConversationId],
        ) -> Result<HashSet<ConversationId>, PersistenceFailure> {
            Ok(values.iter().cloned().collect())
        }
    }
    impl TaskReadCapability for Authority {
        async fn may_read_tasks(
            &mut self,
            _: &AgentId,
            _: &WorkspaceId,
        ) -> Result<bool, PersistenceFailure> {
            Ok(true)
        }
        async fn readable_task_workspaces(
            &mut self,
            _: &AgentId,
            values: &[WorkspaceId],
        ) -> Result<HashSet<WorkspaceId>, PersistenceFailure> {
            Ok(values.iter().cloned().collect())
        }
    }

    fn candidate(
        source: bool,
        source_ref: Option<TaskMessageRef>,
        created_ref: Option<TaskMessageRef>,
    ) -> TaskPointCandidate {
        let task = TaskAggregate::decode(TaskRow {
            id: "task-1".into(),
            task_number: 1,
            conversation_id: "conversation-1".into(),
            thread_root_id: None,
            board_key: "ws:workspace-1".into(),
            workspace_id: Some("workspace-1".into()),
            title: "title".into(),
            content: "body".into(),
            status: "todo".into(),
            assignee_agent_id: None,
            assignee_user_id: None,
            creator_kind: "system".into(),
            creator_agent_id: None,
            creator_user_id: None,
            source_message_id: source.then(|| "source-1".into()),
            created_event_message_id: Some("created-1".into()),
            last_editor_kind: None,
            last_editor_id: None,
            last_edited_at: None,
            created_at: 1,
            closed_at: None,
        })
        .unwrap();
        TaskPointCandidate::new(
            task,
            TaskDisplaySidecars {
                channel_name: Some("General".into()),
                assignee_agent_name: None,
                assignee_user_name: None,
                creator_agent_name: None,
                creator_user_name: None,
            },
            None,
            source_ref,
            created_ref,
        )
    }

    fn message(id: &str) -> TaskMessageRef {
        TaskMessageRef {
            message_id: MessageId::parse(id).unwrap(),
            conversation_id: ConversationId::parse("conversation-1").unwrap(),
            thread_root_id: None,
        }
    }

    #[tokio::test]
    async fn declared_missing_source_never_falls_back_to_created_event() {
        let id = TaskId::parse("task-1").unwrap();
        let viewer = ReadViewer::User(UserId::parse("user-1").unwrap());
        let request = TaskReadRequest {
            address: TaskReadAddress::ById(id),
            actor: TaskReadActor::User(UserId::parse("user-1").unwrap()),
        };
        let mut participation = Authority;
        let visible = authorize_task_point_on(
            &request,
            candidate(true, None, Some(message("created-1"))),
            &mut participation,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            visible.for_viewer(&viewer).unwrap().nav,
            &TaskNavigation::Unavailable
        );
    }

    #[tokio::test]
    async fn source_absence_uses_the_readable_created_event() {
        let id = TaskId::parse("task-1").unwrap();
        let viewer = ReadViewer::User(UserId::parse("user-1").unwrap());
        let request = TaskReadRequest {
            address: TaskReadAddress::ById(id),
            actor: TaskReadActor::User(UserId::parse("user-1").unwrap()),
        };
        let mut participation = Authority;
        let visible = authorize_task_point_on(
            &request,
            candidate(false, None, Some(message("created-1"))),
            &mut participation,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(
            visible.for_viewer(&viewer).unwrap().nav,
            TaskNavigation::CreatedEvent(_)
        ));
    }
}
