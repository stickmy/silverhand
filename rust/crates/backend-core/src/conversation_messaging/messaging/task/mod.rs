//! Task board: a task is a first-class aggregate and a sub-object of exactly
//! ONE conversation (`tasks.conversation_id` is NOT NULL with `ON DELETE
//! CASCADE`) — deleting the conversation deletes its tasks, and a task's board
//! coordinates are frozen from birth. Visibility runs on two independent axes:
//! a read/conversation-visible axis (the chip and `conversation:<id>` events
//! follow channel read access, member or not) and a write/board axis (claim,
//! status, edit, and the per-viewer board fanout require placement
//! membership). Both axes always derive from the task's OWN conversation,
//! never its board.
//!
//! This slice is the value-object / ubiquitous-language layer, the pure
//! mutation decisions that ride it, and the committed-row aggregate the read and
//! write paths share: the validated domain values every later consumer (write
//! path, read models, wire projection) imports, the `status` verb's two doors
//! with their `closed_at` coupling, the `task edit` canonical no-op guard, and
//! [`TaskAggregate`] with its strict [`TaskAggregate::decode`] boundary — the
//! corruption gate that turns a `tasks` row violating any ROW-LOCAL invariant a
//! correct writer upholds into a named safe-500 before any DTO, witness, or
//! refusal is built. That seal is row-local by design: the cross-row check that
//! the board coordinate agrees with the conversation's placement needs a SECOND
//! row's authority and is a hard gate the SQLite read/UoW owns, not this core
//! decode.
//! Both mutation decisions hand back an opaque witness ([`TaskStatusChange`],
//! [`TaskEditChange`]) that only their planner can mint, so a write path
//! expresses "I was authorized to change this" in its signature instead of
//! trusting every caller to have asked first. The read side has its own
//! authorization: [`read_visible_task_on`] is the whole "may see this task" authority
//! over one decoded task. It takes a [`TaskReadRequest`] — the [`TaskReadAddress`] that
//! was asked for plus the actor asking — because "the task I got back is the one I
//! asked for" is a property of the REQUEST, not of a reader mode; the address is
//! actor-independent so the human arm cannot silently lack that bind. It composes a
//! closed set of reader modes ([`TaskReadActor`] / [`AgentRead`]: human, agent by-id
//! global, agent board-scoped) with five ANDed gates:
//! identity — the fetched task IS the one the request's address named, for EVERY actor
//! and above the actor dispatch: by id the ids match, by number BOTH the board and the
//! board-local number match, since a number without its board is not a coordinate; a
//! mismatch is a named safe-500, never a denial and never a 404, both of which would
//! report a fact about the wrong task;
//! witness provenance — a coordinate-less [`GlobalAgentRead`] may only accompany the
//! request whose address it was minted for, so a by-number read cannot mint one from a
//! throwaway by-id address and escape the coordinate gate (also a safe-500);
//! board coordinate (via the subject-bound [`AuthorizedAgentBoard`] witness that
//! [`authorize_agent_board`] mints, binding the resolver's echoed subject to the
//! request); placement participation (the narrow [`TaskParticipationReads`] port, not
//! the whole send gate's reads); and the `read_tasks` capability on a workspace board
//! ([`TaskReadCapability`], the parity of `canAgentUseCapability`).
//! Each agent arm is itself an un-forgeable witness — the coordinate-less by-id arm
//! is a [`GlobalAgentRead`] minted only by reading the scope from the authenticated
//! [`RuntimePrincipal`](crate::host_runtime::runtime_credential::RuntimePrincipal) (agent-slot,
//! never a conversation-frame) and the operation from a typed [`TaskReadAddress`]
//! (by-id, never by-number) ([`authorize_global_by_id`]), taking the agent from the
//! principal and RETAINING the requested id so the provenance gate binds the witness to
//! the request the identity gate already bound to the fetched task — so neither a
//! conversation-frame principal, a by-number address, a self-reported scope label, nor a
//! forged `ById(other_id)` (usable only against the one task it names) can skip the
//! coordinate or identity gates. Its success is itself a [`VisibleTask`] witness that retains the authorized
//! [`ReadViewer`] and exposes the aggregate only through the viewer-bound
//! [`VisibleTask::for_viewer`], so a viewer-specific projection binds the subject
//! before projecting instead of dropping it. This module is driver-free
//! and clock-free — `now` is a parameter — and the consumer-defined ports and the
//! SQLite/wire adapters are separate slices.

pub mod agent_list;
pub mod aggregate;
pub mod edit;
pub mod event;
pub mod human_board;
pub mod point_detail;
pub mod read;
pub mod spec;
pub mod status;
pub mod write;
pub mod write_door;

pub use agent_list::{
    TaskCandidateFetch, TaskCandidateQuery, TaskListAccess, TaskListForbidden,
    resolve_task_list_boards,
};
pub use aggregate::{
    BoardKey, CreatedEventMessageId, EmptyCoordinate, LastEdit, LastEditor, PairKey,
    SourceMessageId, TaskAggregate, TaskRow, TaskRowCorruption, ThreadRootId,
};
pub use edit::{EditPlan, TaskEditChange, plan_edit};
pub use event::{
    AgentEventNames, CreatePlacement, TaskEventAssignee, TaskEventFacts, TaskEventKind,
    TaskEventMessage, TaskSystemActor, compose_claim_event, compose_create_event,
    compose_edit_event, compose_status_event, compose_unclaim_event,
};
pub use human_board::{
    HumanTaskBoardRead, TaskBoardAssigneeFilter, TaskBoardColumnPage, TaskBoardColumnRequest,
    TaskBoardContainer, TaskBoardCounts, TaskBoardCreatorFilter, TaskBoardCursor, TaskBoardFacets,
    TaskBoardFilters, TaskBoardPerson,
};
pub use point_detail::{
    TaskAuthorizedPointRead, TaskConversationBoardRead, TaskEditorDisplay, TaskMessageRef,
    TaskNavigation, TaskPointCandidate, TaskPointFetch, TaskPointSource, VisibleTaskPoint,
    authorize_task_point_on,
};
pub use read::{
    AgentBoardResolver, AgentRead, AgentReadableBoards, AuthorizedAgentBoard, AuthorizedTaskBoards,
    BoardAuthorization, BoardResolution, BoardScopeRequest, BoardScopeSource, GlobalAgentRead,
    ReadViewer, ReadableBoard, ResolvedBoard, TaskDisplaySidecars, TaskListCandidate,
    TaskParticipationReads, TaskReadActor, TaskReadAddress, TaskReadCapability, TaskReadRequest,
    TaskSummarySource, VisibleTask, VisibleTaskListRow, authorize_agent_board,
    authorize_global_by_id, authorize_readable_agent_boards, authorize_task_list_boards,
    list_visible_tasks, read_visible_task_on,
};
pub use spec::{
    Assignee, ContentTooLong, Creator, EmptyTaskId, InvalidTaskNumber, InvalidTitle,
    TASK_CONTENT_MAX_LEN, TASK_TITLE_MAX_LEN, TaskContent, TaskId, TaskNumber, TaskStatus,
    TaskTitle, UnknownTaskStatus,
};
pub use status::{
    AgentStatusRefusal, AssignmentRefusal, StatusTransitionRefusal, TaskStatusChange,
    agent_release_status_change, agent_status_change, agent_status_write_decision, assignment_open,
    claim_status_change, human_status_change,
};
pub use write::{
    AgentClaimOutcome, AgentStatusWriteOutcome, ExistingSourceTask, HumanClaimOutcome,
    HumanStatusWriteOutcome, HumanTaskStatusWrite, TaskClaimWrite, TaskCommittedEventMessage,
    TaskCreateAdmissionFailure, TaskCreateAdmissionReads, TaskCreateBatch,
    TaskCreateConversationState, TaskCreateOutcome, TaskCreatePlan, TaskCreateSourceRef,
    TaskCreateWrite, TaskCreatedWitness, TaskCreator, TaskEditSuccess, TaskEditWitness,
    TaskEditWrite, TaskEditWriteOutcome, TaskEditor, TaskMutationWitness, TaskStatusWrite,
    TaskWriteClock, UnclaimOutcome, admit_task_create,
};
pub use write_door::{
    AgentDisplayNameReader, AgentExistenceReader, TaskByIdCapability, TaskNumberResolver,
    TaskWriteCapability, TaskWriteDoorReads, WorkspaceMembership,
};
