//! Post-commit effects for task mutations.
//!
//! A task write commits the row and its `task_*` system message atomically. Only
//! that committed witness can mint a [`TaskAftermathTrigger`]. The trigger also
//! binds request-time actor provenance and the complete effect policy: publish the
//! committed message, patch the owning conversation board, patch viewer boards,
//! wake participants, and (for agent create/claim/unclaim/status) emit the
//! cross-operation audit. Edit excludes the editing agent from wake-up.
//!
//! Privacy provenance (`source_conversation`) lives only on this command object; it
//! is never copied into persisted message metadata. Effects run after commit and a
//! delivery failure is observable but cannot turn the durable mutation into 500.

use silverhand_backend_core::conversation_messaging::messaging::task::{
    AssigneeNotice, BoardKey, TaskEventKind, TaskMutationWitness, TaskWriteDoorReads,
};
use silverhand_backend_core::foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId};
use silverhand_backend_core::foundation::persistence::PersistenceFailure;

/// Request-time placement of an agent runtime's source conversation. This is
/// captured before the mutation enters its writer and then carried through the
/// post-commit boundary; an executor never has to reconstruct audit provenance
/// from a conversation row that may have moved or disappeared after commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSourcePlacementSnapshot {
    conversation_id: ConversationId,
    workspace_id: Option<WorkspaceId>,
}

impl TaskSourcePlacementSnapshot {
    pub fn new(conversation_id: ConversationId, board: Option<&BoardKey>) -> Self {
        Self {
            conversation_id,
            workspace_id: board.and_then(BoardKey::workspace_id).cloned(),
        }
    }

    pub fn conversation_id(&self) -> &ConversationId {
        &self.conversation_id
    }

    pub fn workspace_id(&self) -> Option<&WorkspaceId> {
        self.workspace_id.as_ref()
    }
}

/// Immutable coordinates consumed by the cross-conversation audit leg.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskCrossOperationAudit {
    source: TaskSourcePlacementSnapshot,
    owner_conversation_id: ConversationId,
    owner_workspace_id: Option<WorkspaceId>,
}

impl TaskCrossOperationAudit {
    pub fn source(&self) -> &TaskSourcePlacementSnapshot {
        &self.source
    }

    pub fn owner_conversation_id(&self) -> &ConversationId {
        &self.owner_conversation_id
    }

    pub fn owner_workspace_id(&self) -> Option<&WorkspaceId> {
        self.owner_workspace_id.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskAftermathActor {
    Agent {
        id: AgentId,
        source: Option<TaskSourcePlacementSnapshot>,
    },
    Human {
        id: UserId,
    },
}

impl TaskAftermathActor {
    pub fn agent_id(&self) -> Option<&AgentId> {
        match self {
            Self::Agent { id, .. } => Some(id),
            Self::Human { .. } => None,
        }
    }

    pub fn source_conversation(&self) -> Option<&ConversationId> {
        match self {
            Self::Agent { source, .. } => source
                .as_ref()
                .map(TaskSourcePlacementSnapshot::conversation_id),
            Self::Human { .. } => None,
        }
    }

    pub fn source_workspace(&self) -> Option<&WorkspaceId> {
        match self {
            Self::Agent { source, .. } => source
                .as_ref()
                .and_then(TaskSourcePlacementSnapshot::workspace_id),
            Self::Human { .. } => None,
        }
    }
}

/// The immutable, complete set of post-commit legs for one mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskEffectPlan {
    publish_event: bool,
    patch_conversation_board: bool,
    patch_viewer_boards: bool,
    wake_participants: bool,
    cross_operation_audit: bool,
    excluded_wake_agent: Option<AgentId>,
}

impl TaskEffectPlan {
    pub fn publish_event(&self) -> bool {
        self.publish_event
    }
    pub fn patch_conversation_board(&self) -> bool {
        self.patch_conversation_board
    }
    pub fn patch_viewer_boards(&self) -> bool {
        self.patch_viewer_boards
    }
    pub fn wake_participants(&self) -> bool {
        self.wake_participants
    }
    pub fn cross_operation_audit(&self) -> bool {
        self.cross_operation_audit
    }
    pub fn excluded_wake_agent(&self) -> Option<&AgentId> {
        self.excluded_wake_agent.as_ref()
    }
}

/// A committed mutation plus its runtime actor and precomputed effect policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskAftermathTrigger {
    committed: TaskMutationWitness,
    actor: TaskAftermathActor,
    plan: TaskEffectPlan,
    cross_operation_audit: Option<TaskCrossOperationAudit>,
    assignee_notice: Option<AssigneeNotice>,
}

impl TaskAftermathTrigger {
    pub fn for_agent(
        committed: TaskMutationWitness,
        actor: AgentId,
        source: Option<TaskSourcePlacementSnapshot>,
    ) -> Self {
        let kind = committed.kind;
        let excluded_wake_agent = (kind == TaskEventKind::Edit).then_some(actor.clone());
        let audit_kind = matches!(
            kind,
            TaskEventKind::Create
                | TaskEventKind::Claim
                | TaskEventKind::Unclaim
                | TaskEventKind::Status
        );
        let cross_operation_audit = source
            .as_ref()
            .filter(|source| source.conversation_id() != &committed.conversation_id)
            .filter(|_| audit_kind)
            .map(|source| TaskCrossOperationAudit {
                source: source.clone(),
                owner_conversation_id: committed.conversation_id.clone(),
                owner_workspace_id: committed.board.workspace_id().cloned(),
            });
        Self {
            committed,
            actor: TaskAftermathActor::Agent { id: actor, source },
            plan: TaskEffectPlan {
                publish_event: true,
                patch_conversation_board: true,
                patch_viewer_boards: true,
                wake_participants: true,
                cross_operation_audit: cross_operation_audit.is_some(),
                excluded_wake_agent,
            },
            cross_operation_audit,
            assignee_notice: None,
        }
    }

    pub fn for_human(committed: TaskMutationWitness, actor: UserId) -> Self {
        Self {
            committed,
            actor: TaskAftermathActor::Human { id: actor },
            plan: TaskEffectPlan {
                publish_event: true,
                patch_conversation_board: true,
                patch_viewer_boards: true,
                wake_participants: true,
                cross_operation_audit: false,
                excluded_wake_agent: None,
            },
            cross_operation_audit: None,
            assignee_notice: None,
        }
    }

    /// Attach the human assignee's committed notice so the effects tell them.
    pub fn with_assignee_notice(mut self, notice: Option<AssigneeNotice>) -> Self {
        self.assignee_notice = notice;
        self
    }

    pub fn committed(&self) -> &TaskMutationWitness {
        &self.committed
    }
    pub fn actor(&self) -> &TaskAftermathActor {
        &self.actor
    }
    pub fn plan(&self) -> &TaskEffectPlan {
        &self.plan
    }
    pub fn cross_operation_audit(&self) -> Option<&TaskCrossOperationAudit> {
        self.cross_operation_audit.as_ref()
    }
    pub fn assignee_notice(&self) -> Option<&AssigneeNotice> {
        self.assignee_notice.as_ref()
    }
}

/// Capture the source conversation's placement before entering the mutation
/// writer. A missing source row is still represented (workspace `None`) because
/// the runtime credential owns the conversation coordinate; the important
/// property here is that post-commit effects never perform a live reconstruction.
pub(crate) async fn capture_task_source<R: TaskWriteDoorReads>(
    reads: &mut R,
    source: Option<&ConversationId>,
) -> Result<Option<TaskSourcePlacementSnapshot>, PersistenceFailure> {
    let Some(source) = source else {
        return Ok(None);
    };
    let board = reads.conversation_board(source).await?;
    Ok(Some(TaskSourcePlacementSnapshot::new(
        source.clone(),
        board.as_ref(),
    )))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AftermathFailure {
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AftermathOutcome {
    Delivered,
    Failed(AftermathFailure),
}

pub trait TaskMutationAftermath: Send + Sync + 'static {
    fn fire(
        &self,
        trigger: &TaskAftermathTrigger,
    ) -> impl std::future::Future<Output = AftermathOutcome> + Send;
}

/// Composition-side executor for every leg in [`TaskEffectPlan`]. Implementations
/// must attempt independent legs even when an earlier one fails and return every
/// non-sensitive failure reason; an empty vector means full delivery.
pub trait TaskEffectExecute: Send + Sync + 'static {
    fn execute_task_effects(
        &self,
        trigger: &TaskAftermathTrigger,
    ) -> impl std::future::Future<Output = Vec<String>> + Send;
}

pub struct PublishingTaskAftermath<E> {
    execute: E,
}

impl<E> PublishingTaskAftermath<E> {
    pub fn new(execute: E) -> Self {
        Self { execute }
    }
}

impl<E: TaskEffectExecute> TaskMutationAftermath for PublishingTaskAftermath<E> {
    async fn fire(&self, trigger: &TaskAftermathTrigger) -> AftermathOutcome {
        let failures = self.execute.execute_task_effects(trigger).await;
        if failures.is_empty() {
            AftermathOutcome::Delivered
        } else {
            AftermathOutcome::Failed(AftermathFailure {
                reason: failures.join("; "),
            })
        }
    }
}

pub(crate) async fn run_aftermath<AF: TaskMutationAftermath>(
    aftermath: &AF,
    trigger: &TaskAftermathTrigger,
) {
    if let AftermathOutcome::Failed(failure) = aftermath.fire(trigger).await {
        tracing::error!(
            task_id = trigger.committed().task_id.as_str(),
            reason = crate::foundation::safe_log_field::safe_display(&(failure.reason)),
            "task mutation committed but its post-commit aftermath failed; the change is durable and the request still succeeds",
        );
    }
}

pub(crate) async fn run_agent_aftermath<AF: TaskMutationAftermath>(
    aftermath: &AF,
    witness: TaskMutationWitness,
    actor: &AgentId,
    source: Option<TaskSourcePlacementSnapshot>,
) {
    let trigger = TaskAftermathTrigger::for_agent(witness, actor.clone(), source);
    run_aftermath(aftermath, &trigger).await;
}

pub(crate) async fn run_human_aftermath<AF: TaskMutationAftermath>(
    aftermath: &AF,
    witness: TaskMutationWitness,
    actor: &UserId,
) {
    let trigger = TaskAftermathTrigger::for_human(witness, actor.clone());
    run_aftermath(aftermath, &trigger).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use silverhand_backend_core::conversation_messaging::messaging::task::{
        BoardKey, CreatedEventMessageId, TaskCommittedEventMessage, TaskEventMessage, TaskId,
        TaskNumber, TaskStatus, TaskTitle,
    };
    use silverhand_backend_core::foundation::identity::WorkspaceId;

    fn witness(kind: TaskEventKind) -> TaskMutationWitness {
        TaskMutationWitness {
            kind,
            task_id: TaskId::parse("task-1").unwrap(),
            task_number: TaskNumber::new(7).unwrap(),
            event_message: TaskCommittedEventMessage::new(
                CreatedEventMessageId::parse("message-1").unwrap(),
                ConversationId::parse("conversation-1").unwrap(),
                None,
                TaskEventMessage {
                    content: "event".into(),
                    metadata: "{}".into(),
                },
                1,
                1,
            ),
            conversation_id: ConversationId::parse("conversation-1").unwrap(),
            board: BoardKey::Workspace(WorkspaceId::parse("workspace-1").unwrap()),
            thread_root_id: None,
            title: TaskTitle::parse("canonical title").unwrap(),
            status: TaskStatus::InProgress,
            assignee: None,
            closed_at: None,
            source_message_id: None,
        }
    }

    #[test]
    fn agent_effect_policy_binds_provenance_audit_and_edit_wake_exclusion() {
        let agent = AgentId::parse("agent-1").unwrap();
        let source = ConversationId::parse("source-1").unwrap();
        let source = TaskSourcePlacementSnapshot::new(
            source.clone(),
            Some(&BoardKey::Workspace(
                WorkspaceId::parse("workspace-source").unwrap(),
            )),
        );
        let status = TaskAftermathTrigger::for_agent(
            witness(TaskEventKind::Status),
            agent.clone(),
            Some(source.clone()),
        );
        assert_eq!(status.actor().agent_id(), Some(&agent));
        assert_eq!(
            status.actor().source_conversation(),
            Some(source.conversation_id())
        );
        assert_eq!(
            status.actor().source_workspace().map(WorkspaceId::as_str),
            Some("workspace-source")
        );
        assert!(status.plan().publish_event());
        assert!(status.plan().patch_conversation_board());
        assert!(status.plan().patch_viewer_boards());
        assert!(status.plan().wake_participants());
        assert!(status.plan().cross_operation_audit());
        let audit = status.cross_operation_audit().unwrap();
        assert_eq!(audit.source(), &source);
        assert_eq!(audit.owner_conversation_id().as_str(), "conversation-1");
        assert_eq!(
            audit.owner_workspace_id().map(WorkspaceId::as_str),
            Some("workspace-1")
        );
        assert_eq!(status.plan().excluded_wake_agent(), None);

        let edit = TaskAftermathTrigger::for_agent(
            witness(TaskEventKind::Edit),
            agent.clone(),
            Some(source),
        );
        assert!(!edit.plan().cross_operation_audit());
        assert_eq!(edit.cross_operation_audit(), None);
        assert_eq!(edit.plan().excluded_wake_agent(), Some(&agent));
    }

    #[test]
    fn human_effect_policy_has_no_private_origin_or_cross_operation_audit() {
        let trigger = TaskAftermathTrigger::for_human(
            witness(TaskEventKind::Create),
            UserId::parse("user-1").unwrap(),
        );
        assert_eq!(trigger.actor().source_conversation(), None);
        assert!(!trigger.plan().cross_operation_audit());
        assert_eq!(trigger.plan().excluded_wake_agent(), None);
    }

    #[test]
    fn same_conversation_and_agent_slot_operations_have_no_cross_op_audit_leg() {
        let actor = AgentId::parse("agent-1").unwrap();
        let same = TaskSourcePlacementSnapshot::new(
            ConversationId::parse("conversation-1").unwrap(),
            Some(&BoardKey::Workspace(
                WorkspaceId::parse("workspace-1").unwrap(),
            )),
        );
        for source in [Some(same), None] {
            let trigger = TaskAftermathTrigger::for_agent(
                witness(TaskEventKind::Claim),
                actor.clone(),
                source,
            );
            assert!(!trigger.plan().cross_operation_audit());
            assert_eq!(trigger.cross_operation_audit(), None);
        }
    }
}
