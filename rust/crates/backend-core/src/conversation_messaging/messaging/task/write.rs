//! The task write unit-of-work ports — the durable half of the task mutation
//! use-cases (`status`, `edit`, `claim`, `unclaim`, `create`).
//!
//! The pure mutation DECISIONS live in their own leaves ([`super::status`],
//! [`super::edit`]). A write UoW COMMITS such a decision, but the decision is made
//! INSIDE the write transaction over the row read under the lock — never
//! pre-computed and replayed. This is the concurrency contract pinned: a
//! conditional write predicated only on the column it writes (`status`) would let
//! a concurrent claim/unclaim move `assignee` without moving `status` and a
//! stale-authorized status change would still apply. So the write authority
//! re-reads EVERY decision-relevant column under the lock and re-runs the decision
//! there; the "authorized witness" is minted in-transaction, not carried in.
//!
//! This slice defines the STATE-MACHINE persistence + the in-transaction decision
//! replay for the task's OWN columns (status/assignee/creator/closed_at). The
//! concrete adapter ALSO re-decides per-task PARTICIPATION and direct-pair
//! READ-ONLY inside the same write transaction, on the same connection, by reusing
//! the crate's existing transaction-bound authorities (the shared participation
//! predicate and the connection-bound conversation/endpoint reader) — same-snapshot
//! SQL, never the send lane's async `assert_post_authority` awaited under the lock,
//! and never a duplicated predicate. The ORDER is the send/TS order: participation
//! → read-only → task-row.
//!
//! Agent writes additionally replay actor existence and the task-owning workspace's
//! required capability (`write_tasks`, plus `read_tasks` for edit) on that same
//! reserved connection. Agent-side workspace selection remains a door concern, but
//! its preflight verdict is never a write credential. The `task_*` system message is
//! appended atomically with the row mutation; only the external publish/patch/wake/
//! audit aftermath stays post-commit, driven from the committed witness. Refusal,
//! read-only, and not-found outcomes append no event and trigger no aftermath.

use std::future::Future;

use crate::foundation::identity::{AgentId, ConversationId, UserId};
use crate::foundation::persistence::PersistenceFailure;

use super::aggregate::{BoardKey, CreatedEventMessageId, SourceMessageId, ThreadRootId};
use super::event::{
    AgentEventNames, CreatePlacement, TaskEventAssignee, TaskEventKind, TaskEventMessage,
    TaskSystemActor,
};
use super::spec::{TaskContent, TaskId, TaskNumber, TaskStatus, TaskTitle};
use super::status::AgentStatusRefusal;

/// Lifecycle of a task-create destination, read before the write UoW.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskCreateConversationState {
    Open,
    Archived,
}

/// The only source-message fact create admission needs: identity plus owning
/// conversation. A message is never treated as a task aggregate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskCreateSourceRef {
    pub id: SourceMessageId,
    pub conversation_id: ConversationId,
}

/// Canonical persistence reads consumed by task-create admission. The port owns no
/// HTTP taxonomy and no task mutation; it only exposes lifecycle, source location,
/// and the permanent source→task binding.
pub trait TaskCreateAdmissionReads {
    fn conversation_state(
        &self,
        conversation: &ConversationId,
    ) -> impl Future<Output = Result<Option<TaskCreateConversationState>, PersistenceFailure>> + Send;

    fn source_message(
        &self,
        source: &SourceMessageId,
    ) -> impl Future<Output = Result<Option<TaskCreateSourceRef>, PersistenceFailure>> + Send;

    fn task_for_source(
        &self,
        source: &SourceMessageId,
    ) -> impl Future<Output = Result<Option<ExistingSourceTask>, PersistenceFailure>> + Send;

    /// Whether the creator agent aggregate still exists. Human creators do not
    /// use this axis. This is deliberately distinct from name lookup: a display
    /// fallback is not an aggregate-existence credential.
    fn agent_exists(
        &self,
        agent: &AgentId,
    ) -> impl Future<Output = Result<bool, PersistenceFailure>> + Send;
}

/// A request-level create refusal. These are settled before opening the batch write
/// transaction; the source-conflict check is repeated by the UNIQUE constraint in the
/// UoW to close the race.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskCreateAdmissionFailure {
    ConversationNotFound,
    ConversationArchived,
    AgentNotFound,
    NotConversationMember,
    SourceMessageNotFound,
    SourceConversationMismatch,
    SourceNotAccessible,
    SourceConflict { existing_task: ExistingSourceTask },
}

/// Validate one create batch against the destination lifecycle/participation and
/// every declared source message. The order is product-visible and matches
/// `TaskService.createTasks`: lifecycle → agent existence → destination participation → for each source,
/// existence → same-conversation → actor visibility → existing binding.
pub async fn admit_task_create<R, P>(
    actor: &TaskCreator,
    conversation: &ConversationId,
    sources: &[SourceMessageId],
    reads: &R,
    participation: &mut P,
) -> Result<Result<(), TaskCreateAdmissionFailure>, PersistenceFailure>
where
    R: TaskCreateAdmissionReads,
    P: super::read::TaskParticipationReads,
{
    match reads.conversation_state(conversation).await? {
        None => return Ok(Err(TaskCreateAdmissionFailure::ConversationNotFound)),
        Some(TaskCreateConversationState::Archived) => {
            return Ok(Err(TaskCreateAdmissionFailure::ConversationArchived));
        }
        Some(TaskCreateConversationState::Open) => {}
    }

    if let TaskCreator::Agent(agent) = actor
        && !reads.agent_exists(agent).await?
    {
        return Ok(Err(TaskCreateAdmissionFailure::AgentNotFound));
    }

    let destination_accessible = match actor {
        TaskCreator::Agent(agent) => {
            participation
                .is_agent_participant(conversation, agent)
                .await?
        }
        TaskCreator::Human(user) => {
            participation
                .is_user_participant(conversation, user)
                .await?
        }
    };
    if !destination_accessible {
        return Ok(Err(TaskCreateAdmissionFailure::NotConversationMember));
    }

    for source in sources {
        let Some(reference) = reads.source_message(source).await? else {
            return Ok(Err(TaskCreateAdmissionFailure::SourceMessageNotFound));
        };
        if reference.conversation_id != *conversation {
            return Ok(Err(TaskCreateAdmissionFailure::SourceConversationMismatch));
        }
        let accessible = match actor {
            TaskCreator::Agent(agent) => {
                participation
                    .is_agent_participant(&reference.conversation_id, agent)
                    .await?
            }
            TaskCreator::Human(user) => {
                participation
                    .is_user_participant(&reference.conversation_id, user)
                    .await?
            }
        };
        if !accessible {
            return Ok(Err(TaskCreateAdmissionFailure::SourceNotAccessible));
        }
        if let Some(existing_task) = reads.task_for_source(source).await? {
            return Ok(Err(TaskCreateAdmissionFailure::SourceConflict {
                existing_task,
            }));
        }
    }
    Ok(Ok(()))
}

/// The committed identity a successful task MUTATION produced, bound to the SAME
/// durable fact as the row change: the `task_*` system message appended in the same
/// transaction. Post-commit aftermath (publish/board-patch/wake) may consume ONLY
/// this — it is minted solely by a committed change (never a no-op, refusal, or a
/// concurrency loser), so a caller "I changed this and it is durable" cannot be
/// forged from an uncommitted attempt. Shared by every mutation verb
/// (status/claim/unclaim/edit; create has its richer [`TaskCreatedWitness`]).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskCommittedEventMessage {
    pub id: CreatedEventMessageId,
    pub conversation_id: ConversationId,
    pub thread_root_id: Option<ThreadRootId>,
    pub message: TaskEventMessage,
    pub seq: i64,
    pub created_at: i64,
}

impl TaskCommittedEventMessage {
    pub fn new(
        id: CreatedEventMessageId,
        conversation_id: ConversationId,
        thread_root_id: Option<ThreadRootId>,
        message: TaskEventMessage,
        seq: i64,
        created_at: i64,
    ) -> Self {
        Self {
            id,
            conversation_id,
            thread_root_id,
            message,
            seq,
            created_at,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskMutationWitness {
    pub kind: TaskEventKind,
    pub task_id: TaskId,
    pub task_number: TaskNumber,
    /// The `task_*` system message appended in the same reserved-writer transaction.
    /// Full immutable system-message post-image. Effects publish this value
    /// directly; they never re-read a live row by id after commit.
    pub event_message: TaskCommittedEventMessage,
    pub conversation_id: ConversationId,
    pub board: BoardKey,
    pub thread_root_id: Option<ThreadRootId>,
    pub title: TaskTitle,
    pub status: TaskStatus,
    pub assignee: Option<TaskEventAssignee>,
    pub closed_at: Option<i64>,
    pub source_message_id: Option<SourceMessageId>,
}

/// The outcome of an AGENT status write. Every arm is decided from the row read
/// under the write lock: the refusals are the same envelopes `updateStatusAgent`
/// returns, and `Applied` means the fresh row authorized and transitioned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentStatusWriteOutcome {
    /// The fresh row authorized the actor and permitted the transition; the row was
    /// updated (`status` + `closed_at`) AND the `task_status` system message appended
    /// in the same transaction. The witness binds the change to its notice.
    Applied(Box<TaskMutationWitness>),
    /// The authenticated runtime principal no longer names a persisted agent.
    /// Replayed inside the reserved writer so a stale credential cannot commit.
    AgentNotFound,
    /// No task with that id, OR the actor is not a participant of the task's
    /// conversation. The two collapse to ONE anti-enumeration envelope (TS B2): a
    /// write-face ACL miss must not leak that the task exists. No row written.
    NotFound,
    /// The task's conversation is a direct pair whose peer endpoint is gone
    /// (deleted or its principal removed) — read-only, no further writes. No row
    /// written. Judged AFTER participation, so only a participant learns the peer
    /// left (the send gate's ordering).
    ConversationReadonly,
    /// The row's CURRENT facts refused the change — a status the transition table
    /// rejects, an already-at-status no-op, or an actor who is neither the
    /// assignee nor the creator of an unassigned self-created task. No row written.
    Refused(AgentStatusRefusal),
}

/// Commit an AGENT `status` change to exactly one task by id, deciding the whole
/// authorization + transition INSIDE the write transaction, and — on a real change —
/// appending the `task_status` system message in the SAME transaction so the notice
/// commits atomically with the row (a refusal/loser appends nothing).
///
/// The adapter opens a reserved-writer transaction, re-reads the task's
/// decision-relevant columns (`status`, both assignee columns, creator kind/id,
/// `closed_at`) under the lock, runs
/// [`agent_status_write_decision`](super::status::agent_status_write_decision) on
/// THOSE facts, and only then writes — so a concurrent claim/unclaim that changed
/// `assignee` while leaving `status` unchanged is observed, and a stale
/// authorization cannot apply. `actor_name` is the actor's DB-resolved display name
/// for the event's actor pill (a pure value the door read). The adapter additionally
/// re-decides actor existence, participation, direct-pair read-only, and the
/// task-owning workspace's `write_tasks` capability under the same lock.
pub trait TaskStatusWrite {
    fn apply_agent_status(
        &self,
        task_id: &TaskId,
        actor: &AgentId,
        actor_names: &AgentEventNames,
        target: TaskStatus,
    ) -> impl Future<Output = Result<AgentStatusWriteOutcome, PersistenceFailure>> + Send;
}

/// The outcome of a HUMAN status write. The human door is an OVERRIDE: no
/// transition table and no assignee gate — only a same-status no-op refusal, after
/// the participation + read-only gates. Every arm is decided from the row read
/// under the write lock.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HumanStatusWriteOutcome {
    /// Participation + read-only passed and the status changed; the row was updated
    /// AND the `task_status` system message appended in the same transaction.
    Applied(Box<TaskMutationWitness>),
    /// No task with that id, OR the user is not a participant of the task's
    /// conversation — ONE anti-enumeration envelope. No row written.
    NotFound,
    /// The task's direct-pair conversation is read-only (a peer endpoint is gone).
    /// No row written. Judged after participation.
    ConversationReadonly,
    /// The task already has the requested status — a no-op the human door refuses
    /// (`already_at_status`). No row written.
    AlreadyAtStatus,
}

/// Commit a HUMAN `status` override to exactly one task by id, deciding the whole
/// gate INSIDE the write transaction (participation → read-only → same-status
/// no-op), appending the `task_status` notice in the SAME transaction on a real
/// change. The human door reads NO assignee/creator/transition facts — it is an
/// override — so its decision-relevant mutable set is `status` (the already-at
/// check), plus the `conversation_id` the participation and read-only reads use,
/// all re-read under the lock on the same connection. A human actor's event pill is
/// the literal `user`, so this door needs no `actor_name`.
pub trait HumanTaskStatusWrite {
    fn apply_human_status(
        &self,
        task_id: &TaskId,
        actor: &UserId,
        target: TaskStatus,
    ) -> impl Future<Output = Result<HumanStatusWriteOutcome, PersistenceFailure>> + Send;
}

/// The actor editing a task. The kind drives BOTH the participation reader (agent
/// vs user predicate) and the `last_editor` triple stamped on a real change. Edit
/// has NO assignee gate for either kind — it is authorized by participation (and,
/// for an agent, the `write_tasks`/`read_tasks` capability the door settles).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskEditor {
    Agent(AgentId),
    Human(UserId),
}

/// The committed identity a successful edit produced — the shared
/// [`TaskMutationWitness`] (the appended `task_edit` system message + the changed
/// row). Named for the edit face's callers; every mutation verb shares the type.
pub type TaskEditWitness = TaskMutationWitness;

/// A viewer-bound post-image captured by the edit transaction. The HTTP adapter
/// may project it after commit without re-reading mutable authorization facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskEditSuccess {
    pub detail: super::point_detail::VisibleTaskPoint,
    pub witness: Option<Box<TaskEditWitness>>,
}

/// The outcome of a task `edit` write. Every arm is decided from the row read
/// under the write lock, in the TS `applyTaskEdit` order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskEditWriteOutcome {
    /// A canonical field changed; the row was updated, `last_editor` stamped, AND
    /// the `task_edit` system message appended — all in the one reserved-writer
    /// transaction, so the notice can never be lost while the change commits. The
    /// witness binds the change to its appended message for post-commit publish.
    Applied(TaskEditSuccess),
    /// Agent editor only: its aggregate row disappeared after route admission.
    AgentNotFound,
    /// No task with that id, OR the editor is not a participant of the task's
    /// conversation — one anti-enumeration envelope. No row written.
    NotFound,
    /// The task's direct-pair conversation is read-only (a peer endpoint is gone).
    /// The edit writes a `task_edit` event into the pair, which a read-only pair
    /// refuses. No row written. Judged after participation.
    ConversationReadonly,
    /// The request changed no canonical field (both absent, or each equal to the
    /// current value): a full no-op — no UPDATE, no `last_editor` stamp, no event.
    /// The current detail is still the caller's answer.
    NoOp(TaskEditSuccess),
}

/// Commit a task `edit` to exactly one task by id, deciding the whole gate INSIDE
/// the write transaction: participation → read-only → the canonical no-op guard
/// ([`plan_edit`](super::edit::plan_edit)) over the fresh `title`/`content`. On a
/// real change the row's `title`/`content` are `COALESCE`-bound (a `None` field
/// left unchanged), the `last_editor` triple is stamped, AND the `task_edit` system
/// message is appended over the post-change snapshot — the row UPDATE and its notice
/// commit atomically (a no-op/refusal/rollback writes zero event).
///
/// `editor_name` is the actor's DB-resolved display name the door read as a pure
/// value (the `task_edit` content's actor pill needs it); the `last_editor` triple
/// itself needs only the editor's id/kind. Edit reads NO assignee/creator/status
/// facts for its DECISION — its decision-relevant mutable set is `title`, `content`,
/// and the `conversation_id` the gates use — but the appended event records the
/// post-change status/assignee snapshot. Agent adapters replay both `write_tasks`
/// and `read_tasks` on the task-owning workspace under the reserved writer.
pub trait TaskEditWrite {
    fn apply_edit(
        &self,
        task_id: &TaskId,
        editor: &TaskEditor,
        editor_names: Option<&AgentEventNames>,
        requested_title: Option<TaskTitle>,
        requested_content: Option<TaskContent>,
    ) -> impl Future<Output = Result<TaskEditWriteOutcome, PersistenceFailure>> + Send;
}

/// The outcome of an AGENT self-`claim`. Decided from the row read under the write
/// lock, in TS `claimTaskAgent` order (participation → read-only → done/closed →
/// atomic assign-if-free). `Claimed` carries the resulting status (`todo` →
/// `in_progress`, else unchanged) for the caller's event/ack.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentClaimOutcome {
    /// The task was free and not terminal; assigned to the actor. The resulting
    /// status is `todo` → `in_progress`, else the current status unchanged. The
    /// `task_claim` system message was appended in the same transaction.
    Claimed {
        status: TaskStatus,
        witness: Box<TaskMutationWitness>,
    },
    AgentNotFound,
    /// No task with that id, or the actor is not a participant — one anti-
    /// enumeration miss. No row written.
    NotFound,
    /// The task's direct-pair conversation is read-only. No row written.
    ConversationReadonly,
    /// The task is terminal (`done`/`closed`) — not claimable. No row written.
    TaskDone,
    TaskClosed,
    /// The task already has an assignee (agent or user). No row written.
    AlreadyClaimed,
}

/// A HUMAN `claim` assigns a chosen AGENT to a task. Same gate as the agent path
/// plus the assignee-agent must itself participate in the task's conversation
/// (`assignee_not_conversation_member`), because a fire dispatches to that agent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HumanClaimOutcome {
    Claimed {
        status: TaskStatus,
        witness: Box<TaskMutationWitness>,
    },
    NotFound,
    ConversationReadonly,
    TaskDone,
    TaskClosed,
    AlreadyClaimed,
    /// The chosen assignee agent is not a participant of the task's conversation.
    AssigneeNotConversationMember,
}

/// The outcome shared by two distinct unclaim commands. Agent self-release clears
/// assignment and rewinds workflow to `todo`; human override clears assignment only.
/// Both refuse a terminal task (`done`/`closed`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UnclaimOutcome {
    /// The command-specific post-image was committed and the `task_unclaim` system
    /// message appended in the same transaction.
    Unclaimed(Box<TaskMutationWitness>),
    /// Agent arm only: the actor aggregate disappeared before linearization.
    AgentNotFound,
    NotFound,
    ConversationReadonly,
    TaskDone,
    TaskClosed,
    /// AGENT door only: the actor is not the task's assignee. No row written.
    NotAssignee,
}

/// The agent/human `claim` and `unclaim` write authorities. Each opens ONE
/// `BEGIN IMMEDIATE`, re-reads the decision-relevant columns under the lock, runs
/// the shared participation → read-only gate, then the assignment CAS — a
/// concurrent claim/unclaim cannot be clobbered because the assign/clear is
/// predicated on the current assignee/status. On a committed change each appends its
/// `task_claim`/`task_unclaim` system message in the SAME transaction and returns a
/// witness. `actor_name` (agent arms) / `assignee_name` (human claim) are the
/// DB-resolved names the event's content line needs, read by the door as pure
/// values; a user actor's own pill is the literal `user`.
pub trait TaskClaimWrite {
    /// AGENT self-claim: assign the actor to a free, non-terminal task it
    /// participates in (`todo` → `in_progress`). Claim stamps no timestamp column
    /// (the `tasks` row has none for assignment), so it takes no `now` for the row —
    /// but the appended event's message needs a timestamp, so `now` is passed.
    fn agent_claim(
        &self,
        task_id: &TaskId,
        actor: &AgentId,
        actor_names: &AgentEventNames,
    ) -> impl Future<Output = Result<AgentClaimOutcome, PersistenceFailure>> + Send;

    /// HUMAN claim: assign or reassign the chosen participating `assignee` agent on
    /// a non-terminal task the acting user participates in.
    fn human_claim(
        &self,
        task_id: &TaskId,
        actor: &UserId,
        assignee: &AgentId,
    ) -> impl Future<Output = Result<HumanClaimOutcome, PersistenceFailure>> + Send;

    /// AGENT unclaim: if the actor IS the assignee and the task is non-terminal,
    /// clear assignment, rewind to `todo`, and clear `closed_at` atomically.
    fn agent_unclaim(
        &self,
        task_id: &TaskId,
        actor: &AgentId,
    ) -> impl Future<Output = Result<UnclaimOutcome, PersistenceFailure>> + Send;

    /// HUMAN unclaim: clear the assignee (ownership-agnostic) on a non-terminal task
    /// the user participates in.
    fn human_unclaim(
        &self,
        task_id: &TaskId,
        actor: &UserId,
    ) -> impl Future<Output = Result<UnclaimOutcome, PersistenceFailure>> + Send;
}

/// The creator recorded on a new task row (`creator_kind` + its id column).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskCreator {
    Agent(AgentId),
    Human(UserId),
}

/// One per-task item inside a destination-bound create batch: the row fields plus the pre-composed
/// `task_create` system-message content and metadata JSON (the door builds those,
/// which need DB-backed actor/assignee names resolved before the write). Destination,
/// board, and creator deliberately do not live here: they are bound once by
/// [`TaskCreateBatch`], so no later item can borrow the first item's authority.
/// The task number is allocated by the writer under the lock, so it is NOT an input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskCreatePlan {
    pub thread_root_id: Option<ThreadRootId>,
    pub title: TaskTitle,
    pub content: TaskContent,
    /// `Some` self-claims the created task to this agent (`todo` → `in_progress`);
    /// `None` files it unassigned.
    pub claim_by: Option<AgentId>,
    /// The permanent 1:1 provenance link; a duplicate is a `source_conflict`.
    pub source_message_id: Option<SourceMessageId>,
    /// The `task_create` event's actor pill (the door resolved the display name).
    pub event_actor: TaskSystemActor,
    /// The `task_create` event's assignee DTO (`Some` iff self-claimed at create,
    /// the door resolved the claiming agent's name), or `None` for an unassigned
    /// create. The writer composes the event AFTER allocating the number, so the
    /// content/metadata carry the real `#N` (the door cannot know it pre-tx).
    pub event_assignee: Option<TaskEventAssignee>,
    /// How the create was placed relative to the actor's own conversation — selects
    /// the content template + the `cross_conversation` metadata flag.
    pub event_placement: CreatePlacement,
}

/// An indivisible create batch with its authority-bearing coordinates bound exactly
/// once. Items contain only per-task facts, making a mixed destination, board, or
/// creator unrepresentable at the persistence boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskCreateBatch {
    conversation_id: ConversationId,
    board: BoardKey,
    creator: TaskCreator,
    items: Vec<TaskCreatePlan>,
}

impl TaskCreateBatch {
    pub fn new(
        conversation_id: ConversationId,
        board: BoardKey,
        creator: TaskCreator,
        items: Vec<TaskCreatePlan>,
    ) -> Self {
        Self {
            conversation_id,
            board,
            creator,
            items,
        }
    }

    pub fn conversation_id(&self) -> &ConversationId {
        &self.conversation_id
    }

    pub fn board(&self) -> &BoardKey {
        &self.board
    }

    pub fn creator(&self) -> &TaskCreator {
        &self.creator
    }

    pub fn items(&self) -> &[TaskCreatePlan] {
        &self.items
    }
}

/// The committed identity a successful create produced, all bound to the SAME
/// durable fact: the task, its allocated number, its title, the `task_create`
/// message that anchors it, and the board/conversation it lives on. Post-commit
/// publication may consume ONLY this; it is minted solely by a committed create.
/// `title` rides the witness so the ACK returns `{id, task_number, title}` without a
/// re-read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskCreatedWitness {
    mutation: TaskMutationWitness,
}

impl TaskCreatedWitness {
    pub fn new(mutation: TaskMutationWitness) -> Option<Self> {
        (mutation.kind == TaskEventKind::Create).then_some(Self { mutation })
    }

    pub fn mutation(&self) -> &TaskMutationWitness {
        &self.mutation
    }

    pub fn task_id(&self) -> &TaskId {
        &self.mutation.task_id
    }

    pub fn task_number(&self) -> TaskNumber {
        self.mutation.task_number
    }

    pub fn title(&self) -> &TaskTitle {
        &self.mutation.title
    }
}

/// The task an existing `source_message_id` already backs — surfaced with a
/// `SourceConflict` so the caller can point the operator at the winner (the TS
/// `existing_task`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExistingSourceTask {
    pub task_id: TaskId,
    pub task_number: TaskNumber,
    pub status: TaskStatus,
}

/// The outcome of a BATCH task create — the whole `tasks[]` is ONE transaction, so a
/// refusal means NOTHING was written (no row, no event) and the batch rolls back.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskCreateOutcome {
    /// Every task in the batch committed as one fact; one witness per task, in
    /// input order, each carrying its allocated number + title for the ACK.
    Created(Vec<TaskCreatedWitness>),
    /// Agent creator only: the creator aggregate does not exist at the batch's
    /// reserved-writer boundary. The whole batch remains unwritten.
    AgentNotFound,
    /// A `source_message_id` (in any batch item, or a duplicate WITHIN the batch)
    /// is already bound to a task — the permanent 1:1 provenance rule. The WHOLE
    /// batch rolled back; `existing_task` names the winner when it could be read.
    SourceConflict {
        existing_task: Option<ExistingSourceTask>,
    },
    SourceMessageNotFound,
    SourceConversationMismatch,
    SourceNotAccessible,
    CapabilityDenied(super::write_door::TaskWriteCapability),
    ConversationNotFound,
    ConversationArchived,
    NotConversationMember,
    /// The target conversation is a direct pair whose peer endpoint is gone: it
    /// admits no writer, and a `task_create` message cannot be appended. Nothing
    /// written.
    ConversationReadonly,
}

/// Commit a BATCH task creation as ONE indivisible durable fact: the writer opens a
/// SINGLE reserved-writer transaction and, for EACH item in order, allocates the next
/// per-board task number, inserts the row, appends the `task_create` message, binds
/// `created_event_message_id` — then commits the whole batch. Any step failing (a
/// duplicate `source_message_id` in any item, incl. two items naming the same source)
/// rolls the WHOLE batch back, so a partial batch can never commit and no per-item
/// aftermath fires before the batch is durable. A duplicate source is a typed
/// `SourceConflict` (with the winner when readable); a read-only destination is
/// `ConversationReadonly`.
///
/// The route performs an ordered preflight for early canonical refusals, but this
/// writer freshly replays creator existence, destination lifecycle/participation,
/// workspace capabilities, and every source-message fact under its reserved writer
/// before inserting anything. The typed batch binds destination, board, and creator
/// once; per-task items cannot override them.
pub trait TaskCreateWrite {
    fn create_tasks(
        &self,
        batch: &TaskCreateBatch,
    ) -> impl Future<Output = Result<TaskCreateOutcome, PersistenceFailure>> + Send;
}
/// Transactional task-write clock. Adapters sample it exactly once after
/// reserving the writer; routes never carry a pre-lock instant as authority.
pub trait TaskWriteClock {
    fn now(&self) -> i64;
}
