//! Driver-free contracts for the bounded human task board reads.

use std::future::Future;

use crate::foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId};
use crate::foundation::persistence::PersistenceFailure;
use crate::foundation::wire_number::JsSafeInteger;

use super::{TaskId, TaskListCandidate, TaskNumber, TaskStatus};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskBoardContainer {
    Workspace,
    AgentSpace,
    DirectMessages,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskBoardAssigneeFilter {
    Agent(AgentId),
    User(UserId),
    Unassigned,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskBoardCreatorFilter {
    Agent(AgentId),
    User(UserId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskBoardFilters {
    pub viewer: UserId,
    pub workspace: Option<WorkspaceId>,
    pub conversation: Option<ConversationId>,
    pub container: Option<TaskBoardContainer>,
    pub assignee: Option<TaskBoardAssigneeFilter>,
    pub creator: Option<TaskBoardCreatorFilter>,
    /// Only tasks created at or after this epoch-millisecond instant.
    pub created_after: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskBoardCursor {
    pub task_number: TaskNumber,
    pub task_id: TaskId,
}

pub struct TaskBoardColumnRequest {
    pub filters: TaskBoardFilters,
    pub status: TaskStatus,
    pub cursor: Option<TaskBoardCursor>,
    pub limit: usize,
}

pub struct TaskBoardColumnPage {
    pub rows: Vec<TaskListCandidate>,
    pub has_more: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskBoardCounts {
    pub todo: JsSafeInteger,
    pub in_progress: JsSafeInteger,
    pub in_review: JsSafeInteger,
    pub done: JsSafeInteger,
    pub closed: JsSafeInteger,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskBoardPerson {
    Agent { id: AgentId, name: String },
    User { id: UserId, name: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskBoardFacets {
    pub assignees: Vec<TaskBoardPerson>,
    pub creators: Vec<TaskBoardPerson>,
    pub has_unassigned: bool,
}

pub trait HumanTaskBoardRead {
    fn column(
        &self,
        request: &TaskBoardColumnRequest,
    ) -> impl Future<Output = Result<TaskBoardColumnPage, PersistenceFailure>> + Send;

    fn counts(
        &self,
        filters: &TaskBoardFilters,
    ) -> impl Future<Output = Result<TaskBoardCounts, PersistenceFailure>> + Send;

    /// Facets intentionally honor only viewer + container; within-board filters
    /// never shrink the menu that is needed to switch those filters.
    fn facets(
        &self,
        viewer: &UserId,
        container: Option<TaskBoardContainer>,
    ) -> impl Future<Output = Result<TaskBoardFacets, PersistenceFailure>> + Send;
}
