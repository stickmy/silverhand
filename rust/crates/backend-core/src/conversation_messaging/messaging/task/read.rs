//! The task read authorization: whether a viewer may see one already-decoded
//! task, and the authorization fact minted when they may.
//!
//! This is the whole "may see this task" authority for a single dereferenced
//! task — the parity of TS's `TaskAccessPolicy.canAgentSeeTask(actor, task,
//! 'read_tasks')` composed with the runtime-scope / selector resolution the route
//! (`resolveAgentTaskBoard`) draws before it. Authorization is not a bearer result
//! passed around: every fact here carries its SUBJECT (the principal, and for a
//! board the source it was resolved from), and each boundary binds the subject to
//! the request before trusting it.
//!
//! ## The request: a coordinate plus an actor
//!
//! A read is a [`TaskReadRequest`] — the [`TaskReadAddress`] that was asked for plus
//! the [`TaskReadActor`] asking. The address is deliberately NOT part of the actor: "the
//! task I got back is the one I asked for" is a property of the request, so it is bound
//! once for every reader mode instead of per-arm (where the human arm would silently
//! have no such bind). The SAME address value must drive the lookup and this
//! authorization, and it must never be re-derived from the task the lookup returned.
//!
//! ## The closed set of reader modes
//!
//! A task read has three reader modes, sealed so an illegal one is unconstructable:
//!
//!  - **Human** ([`TaskReadActor::User`]): conversation participation is the WHOLE
//!    authorization. No board coordinate (a human is not scoped to one board) and no
//!    capability (capabilities are an agent-only axis — TS `permissions.ts`). The
//!    request's identity gate still applies: a human by-id or by-number read answered
//!    with a DIFFERENT task is the same miswiring as an agent's, and a human who happens
//!    to participate in that other task must not be handed it.
//!  - **Agent, by-id global** ([`AgentRead::GlobalById`]): an authenticated
//!    agent-slot runtime dereferencing a task BY ID — NO board coordinate. It may
//!    reach a task on ANY board, but participation in the task's own conversation is
//!    still required (never bypassed) and, on a workspace board, the `read_tasks`
//!    capability. This arm is a [`GlobalAgentRead`] witness minted ONLY by
//!    [`authorize_global_by_id`], which reads the scope from the AUTHENTICATED
//!    [`RuntimePrincipal`] (agent-slot = no ambient conversation) and takes the
//!    operation from a typed [`TaskReadAddress`]: a conversation-frame principal
//!    (always board-scoped) or a by-number address (always board-resolved) mints
//!    nothing, and the agent is taken from the principal — so neither a
//!    self-reported scope label nor a bare agent id can pick the coordinate-less arm.
//!    It RETAINS the requested [`TaskId`], so the provenance gate binds the witness to
//!    the REQUEST's address — a witness minted for one id is usable only alongside that
//!    id's request, which the identity gate has already bound to the fetched task. TS
//!    parity: the `runtimeScope === 'agent'` by-id path skips the board-coordinate block
//!    but still runs `canAgentSeeTask`.
//!  - **Agent, board-scoped** ([`AgentRead::BoardScoped`]): a read confined to ONE
//!    membership-authorized board — a conversation-frame's ambient board, a frame's
//!    explicitly-selected other workspace, or an agent-slot's explicitly-selected
//!    workspace. All three are minted by the SAME resolver over
//!    [`BoardScopeSource`], so a coordinate gate applies. TS parity:
//!    `resolveAgentTaskBoard`, which authorizes membership before yielding a board.
//!    An agent-slot by-number read with no selector forms no request at all (a slot
//!    has no ambient frame), so it is refused where the request is built — and it
//!    cannot fall back to `GlobalById`, whose minter refuses a by-number operation.
//!
//! ## The gates, ANDed, in a load-bearing order
//!
//!  1. **Identity** (EVERY reader mode, above the actor dispatch): the task the lookup
//!     RETURNED must be the one the request's address NAMED — by id, the ids match; by
//!     number, BOTH the board and the board-local number match, since a number without
//!     its board is not a coordinate. So the address that decided the lookup and the
//!     address authorization was checked against are the same coordinate, and no reader
//!     mode can be answered with a task it did not ask for. Unlike the gates below, a
//!     mismatch is a safe-500 (a wired-wrong authority), not a denial and not a 404 —
//!     both would report a fact about the WRONG task. It is also what makes forging
//!     `ById(other_id)` useless: the read authorizes ONLY the task that id names,
//!     reachable only by legitimately holding it.
//!  2. **Witness provenance** (the coordinate-less agent arm): a [`GlobalAgentRead`]
//!     may only accompany the request whose address it was minted for. Its sole
//!     privilege is skipping gate 3, so without this bind a by-number read could mint
//!     one from a throwaway `ById` and escape the coordinate gate. Also a safe-500.
//!  3. **Coordinate** (board-scoped only): the task's OWN board — the board the
//!     committed row carries, already sealed against its conversation placement by
//!     the decode path — must equal the read's authorized board. That board is an
//!     [`AuthorizedAgentBoard`], bound to its agent and minted from an authorized
//!     source; `GlobalById` has no coordinate gate.
//!  4. **Participation** (every agent arm): the reader participates in the task's
//!     OWN conversation — placement participation, the SAME authority the send gate
//!     reads, narrowed to [`TaskParticipationReads`]. NEVER bypassed, not even for a
//!     capability-holding global reader. The agent asked about is the one carried by
//!     the actor's scope, never a separately-supplied id.
//!  5. **Capability** (workspace boards only): the agent holds `read_tasks` on the
//!     task's workspace ([`TaskReadCapability`], the parity of
//!     `canAgentUseCapability(..., 'read_tasks')` including the agent-home owner
//!     arm). A direct-pair board has NO capability axis — participation is its whole
//!     authority (TS: `canAgentSeeTask` returns on `workspace_id === null`).
//!
//! Any denial gate's miss returns `None`, so an off-board task, a non-participant, and
//! a capability-less agent are indistinguishable. A `PersistenceFailure` is a
//! safe-500 from an authority (including gates 1 and 2), never a "no".
//!
//! ## Provenance, not bearer tokens
//!
//! An agent read's board is ALWAYS server-resolved and membership-authorized, never
//! client-echoed. [`AuthorizedAgentBoard`] carries the AGENT it was minted for and
//! is produced ONLY by [`authorize_agent_board`], which resolves a
//! [`BoardScopeRequest`] through an [`AgentBoardResolver`] and BINDS the resolver's
//! echoed subject (agent + source) to the request before minting — a resolver that
//! answers for a different subject mints nothing (a safe-500). Because the agent
//! lives inside the witness (and the read gate reads the agent FROM it), a board
//! authorized for agent A cannot be presented as agent B's authorization.
//!
//! The `compile_fail` gate on [`AuthorizedAgentBoard`] is a from-parts negative API
//! gate only — it proves external code cannot recombine a raw `(AgentId, BoardKey)`
//! into one. It is NOT a full proof that authorization cannot be forged: an external
//! crate can still implement [`AgentBoardResolver`] with a fake and call the minter.
//! The full positive proof is the PRODUCTION resolver's subject bind plus the
//! consumer's actor bind ([`VisibleTask::for_viewer`]).
//!
//! ## The success fact keeps its subject
//!
//! [`read_visible_task_on`] returns a [`VisibleTask`] that retains the [`ReadViewer`]
//! the read was authorized for. Its ONLY accessor to the aggregate is
//! [`VisibleTask::for_viewer`], which yields the task only to that same principal —
//! so an A-authorized task cannot drive B's viewer-resolved view. There is no bare
//! degradation exit: a viewer-specific projection must present its own viewer and
//! match, and a genuinely viewer-independent projection should consume the witness
//! through a narrow operation-specific method rather than a generic `TaskAggregate`
//! escape.
//!
//! This is pure domain logic: the fetch that produced the [`TaskAggregate`] and the
//! adapters that answer the ports live in sibling crates; this rule only composes
//! them.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::future::Future;

use crate::foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId};
use crate::foundation::persistence::PersistenceFailure;
use crate::host_runtime::runtime_credential::RuntimePrincipal;

use super::aggregate::{BoardKey, TaskAggregate};
use super::spec::{TaskId, TaskNumber};

/// Where an agent's board scope is resolved FROM. Carried by both the request and
/// the resolver's answer and bound at mint, so a board resolved for one source
/// cannot be spliced onto a request that named another.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BoardScopeSource {
    /// A conversation's own board (TS `boardOfConversation`): a conversation-scoped
    /// runtime's frame, or a `--target` the caller already resolved for this agent.
    FrameConversation(ConversationId),
    /// An explicit `--workspace-id/--workspace` selector (a frame or a slot
    /// runtime): a workspace the agent must be a member of.
    SelectedWorkspace(WorkspaceId),
}

/// A request to resolve the board an agent read is confined to: the agent it is for
/// and the source to resolve from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoardScopeRequest {
    pub agent: AgentId,
    pub source: BoardScopeSource,
}

/// A board-scope authority's answer: the board it resolved, echoing the subject
/// (agent + source) it resolved it FOR so [`authorize_agent_board`] can bind the
/// answer to the request. A correct adapter derives `board` from `source` after
/// authorizing `agent` — the frame's placement, or workspace membership for a
/// selector.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedBoard {
    pub agent: AgentId,
    pub source: BoardScopeSource,
    pub board: BoardKey,
}

/// A board-scope authority's answer for a resolution request. The adapter — the one
/// authority that reads the workspace/placement row — is where the REASON a source
/// resolves to no readable board is known, so it reports the distinction HERE rather
/// than forcing the list surface to run a second existence probe (and open a
/// check-then-act gap). The two refusals let the surface answer `context over control`:
/// an unknown selector is a `404`, an existing-but-unreachable one a `403`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BoardResolution {
    /// The source resolves to a board the agent may target — bound to the request by
    /// [`authorize_agent_board`].
    Resolved(ResolvedBoard),
    /// A [`BoardScopeSource::SelectedWorkspace`] selector that names no such workspace
    /// (`404`). Only a selector can produce this; a frame's source is a real
    /// conversation coordinate.
    NoSuchWorkspace,
    /// The source exists but the agent may not target its board — a non-member of an
    /// existing workspace, or a frame that derives no board (`403`).
    Unauthorized,
}

/// Resolves the board an agent read is confined to. A [`BoardResolution::Resolved`]
/// carries the derived board; [`BoardResolution::NoSuchWorkspace`] and
/// [`BoardResolution::Unauthorized`] are the two refusals — denials, not corruption.
/// The adapter is trusted to derive the board from the source; the core minter binds
/// the echoed subject to the request so a mis-routed or subject-confused answer mints
/// no witness.
pub trait AgentBoardResolver {
    fn resolve_board(
        &mut self,
        request: &BoardScopeRequest,
    ) -> impl Future<Output = Result<BoardResolution, PersistenceFailure>> + Send;
}

/// A board an agent read is authorized against, BOUND to the agent it was minted
/// for.
///
/// It carries the principal, not just the board, so the read gate reads the agent
/// FROM the witness — a witness minted for one agent can never stand in as
/// another's authorization. The fields are private and it is minted only by an
/// agent-bound authority: [`authorize_agent_board`] (a SINGLE board resolved from a
/// source and bound to the request) or [`authorize_readable_agent_boards`] (the union
/// list arm's ENUMERATION, each entry bound to the enumerated agent).
///
/// The gate below is a from-parts negative API gate only (see the module docs): it
/// proves an external crate cannot recombine a raw `(AgentId, BoardKey)` into an
/// authorized board.
///
/// ```compile_fail
/// use silverhand_backend_core::conversation_messaging::messaging::task::{AuthorizedAgentBoard, BoardKey};
/// use silverhand_backend_core::foundation::identity::{AgentId, WorkspaceId};
///
/// // Both fields are private — no external crate can pair an agent with a board to
/// // fabricate an "authorized" one; it can only come from `authorize_agent_board`.
/// let forged = AuthorizedAgentBoard(
///     AgentId::parse("a").unwrap(),
///     BoardKey::Workspace(WorkspaceId::parse("w").unwrap()),
/// );
/// ```
pub struct AuthorizedAgentBoard(AgentId, BoardKey);

impl AuthorizedAgentBoard {
    /// The principal this board was authorized for.
    fn agent(&self) -> &AgentId {
        &self.0
    }

    /// The authorized board.
    fn board(&self) -> &BoardKey {
        &self.1
    }

    /// The server-side lookup coordinate this subject-bound authority resolved.
    pub fn coordinate(&self) -> &BoardKey {
        &self.1
    }
}

/// The typed corruption a subject-confused board-scope authority raises: it answered
/// for a different agent or source than the request named. An authority bug — a
/// safe-500 — never a caller denial.
#[derive(Debug, thiserror::Error)]
#[error("board-scope authority answered for a different subject than requested")]
struct ScopeProvenanceViolation;

/// The outcome of [`authorize_agent_board`]: a subject-bound board witness, or one of
/// the two board-scope refusals carried through from the resolver. The refusal split is
/// the parity of the TS route's `unknown_workspace` (`404`) vs non-member (`403`)
/// distinction — the caller maps them to those statuses.
pub enum BoardAuthorization {
    /// A board the agent is authorized for, bound to the request.
    Authorized(AuthorizedAgentBoard),
    /// A selector that names no such workspace (`404`).
    NoSuchWorkspace,
    /// The source exists but the agent may not read its board (`403`).
    Unauthorized,
}

/// Mint the [`AuthorizedAgentBoard`] a board-scoped agent read is confined to, by
/// resolving `request` and BINDING the answer to it: the resolved subject (agent +
/// source) must equal the requested one, or no witness is produced — a subject
/// mismatch is a safe-500, not a denial. A resolver refusal is carried through verbatim
/// ([`BoardAuthorization::NoSuchWorkspace`]/[`BoardAuthorization::Unauthorized`]) so the
/// list surface can distinguish `404` from `403`.
///
/// This is the ONLY constructor of [`AuthorizedAgentBoard`], so a board enters an
/// agent read only as this bound, request-matched fact.
pub async fn authorize_agent_board<R>(
    request: BoardScopeRequest,
    resolver: &mut R,
) -> Result<BoardAuthorization, PersistenceFailure>
where
    R: AgentBoardResolver,
{
    let resolved = match resolver.resolve_board(&request).await? {
        BoardResolution::Resolved(resolved) => resolved,
        BoardResolution::NoSuchWorkspace => return Ok(BoardAuthorization::NoSuchWorkspace),
        BoardResolution::Unauthorized => return Ok(BoardAuthorization::Unauthorized),
    };
    if resolved.agent != request.agent || resolved.source != request.source {
        return Err(PersistenceFailure::from_source(ScopeProvenanceViolation));
    }
    Ok(BoardAuthorization::Authorized(AuthorizedAgentBoard(
        resolved.agent,
        resolved.board,
    )))
}

/// One board the union list arm ([`authorize_readable_agent_boards`]) enumerated as
/// readable by an agent, echoing the agent it was enumerated FOR so the minter binds the
/// answer to the request. The adapter returns the workspace boards the agent is a
/// member/owner of and the direct-pair boards it is an endpoint of; the `read_tasks`
/// capability is NOT applied here (a direct-pair board has no such axis, and a workspace
/// board's is folded per distinct board by [`authorize_task_list_boards`]).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadableBoard {
    /// The agent this board was enumerated for — bound against the request at mint.
    pub agent: AgentId,
    /// A board the agent may read: a workspace it belongs to or a direct pair it is in.
    pub board: BoardKey,
}

/// Enumerates every board an agent may READ tasks on — the authority behind the UNION
/// list arm (an agent-slot list with no workspace selector), the parity of the TS
/// `workspaces.listForAgent` ∪ `listAgentDirectPairBoardKeys`. It returns the agent's
/// membership/owner workspace boards and the direct-pair boards it is an endpoint of,
/// keyed by the agent, so the enumeration itself is the authorization — there is no
/// per-board source selector to confuse, unlike the single-board [`AgentBoardResolver`].
/// The `read_tasks` capability is folded downstream by [`authorize_task_list_boards`].
pub trait AgentReadableBoards {
    fn readable_boards(
        &mut self,
        agent: &AgentId,
    ) -> impl Future<Output = Result<Vec<ReadableBoard>, PersistenceFailure>> + Send;
}

/// Mint the union arm's [`AuthorizedAgentBoard`] set by enumerating the boards `agent`
/// may read and BINDING each to the requested agent: an entry enumerated for a different
/// agent is a wired-wrong adapter, a safe-500, never a caller denial. The result feeds
/// [`authorize_task_list_boards`], which dedups and folds the `read_tasks` capability, so
/// the union arm reaches the SAME opaque scope the single-board arms do — a list entry
/// point still cannot be handed a raw `Vec<BoardKey>` with an unbacked authorization claim.
/// An empty enumeration is an agent with no readable board, which lists as nothing.
pub async fn authorize_readable_agent_boards<E>(
    agent: &AgentId,
    enumerator: &mut E,
) -> Result<Vec<AuthorizedAgentBoard>, PersistenceFailure>
where
    E: AgentReadableBoards,
{
    let enumerated = enumerator.readable_boards(agent).await?;
    let mut authorized = Vec::with_capacity(enumerated.len());
    for entry in enumerated {
        if &entry.agent != agent {
            return Err(PersistenceFailure::from_source(ScopeProvenanceViolation));
        }
        authorized.push(AuthorizedAgentBoard(entry.agent, entry.board));
    }
    Ok(authorized)
}

/// WHICH task a read named — the request's coordinate, and the SAME value that must
/// drive the lookup. It is deliberately ACTOR-INDEPENDENT: a human by-id read and an
/// agent board-scoped by-number read carry the identical fact, because "the fetched
/// task is the one I asked for" is a property of the request, not of who is asking.
/// The by-id arm carries an opaque [`TaskId`] (a global dereference that needs no
/// board); the by-number arm carries a board-local [`TaskNumber`] AND the board it is
/// local to, because a number without its board is not a coordinate at all.
///
/// The by-number board must come from resolving the REQUEST (an agent's authorized
/// board scope, a human request's conversation placement) — never copied back off the
/// task the lookup returned, which would make the gate compare a value to itself.
///
/// For an agent this is also the operation half of a global read's provenance (the
/// scope half is the authenticated [`RuntimePrincipal`]): a by-number address can never
/// mint the coordinate-less arm.
pub enum TaskReadAddress {
    /// By opaque id — a global dereference that needs no board.
    ById(TaskId),
    /// By board-local number, with the board that number is local to. Only resolves
    /// against a board, so it can never take the coordinate-less path.
    ByNumber { board: BoardKey, number: TaskNumber },
}

impl TaskReadAddress {
    /// Whether `task` is the task this address names — the identity gate's whole
    /// question. By id: the ids match. By number: BOTH the board and the board-local
    /// number match, since a number is only meaningful within its board.
    fn names(&self, task: &TaskAggregate) -> bool {
        match self {
            TaskReadAddress::ById(id) => task.id() == id,
            TaskReadAddress::ByNumber { board, number } => {
                task.board() == board && task.task_number() == *number
            }
        }
    }
}

/// One task read: the coordinate that was asked for, and who asked. Holding the two
/// together is what lets a call site drive the lookup and this authorization from the
/// SAME address value instead of re-deriving one of them — the structural half of the
/// identity gate below (the other half is that the address is never reconstructed from
/// the returned task).
pub struct TaskReadRequest {
    pub address: TaskReadAddress,
    pub actor: TaskReadActor,
}

/// The typed corruption raised when the task a lookup RETURNED is not the one the
/// request's address NAMED. The address that decided the lookup and the address the
/// authorization was checked against must be the same coordinate, for EVERY actor: a
/// by-id read authorizes only the task whose id it names; a by-number read only the
/// task at that board-local number. A mismatch means the lookup answered off its key
/// (a wired-wrong adapter, a confused call site) — an authority bug, a safe-500, never
/// a caller denial, and never a 404 (a 404 would report a fact about the WRONG task).
/// Binding it is also what makes a forged `ById(other_id)` useless: it authorizes ONLY
/// the task that id names, reachable only by legitimately holding that id.
#[derive(Debug, thiserror::Error)]
#[error("task lookup returned a different task than the requested address named")]
struct AddressIdentityViolation;

/// The typed corruption raised when a coordinate-less [`GlobalAgentRead`] witness is
/// presented alongside a request whose address is NOT the one it was minted for. The
/// witness's only privilege is skipping the board-coordinate gate, so letting it ride
/// an unrelated address would let a by-number read (minted from a throwaway by-id
/// address) escape that gate. A call-site bug — a safe-500, never a denial.
#[derive(Debug, thiserror::Error)]
#[error("coordinate-less read witness was minted for a different address than requested")]
struct AddressProvenanceViolation;

/// Proof that an agent read runs WITHOUT a board coordinate: an authenticated
/// agent-slot runtime (never a conversation-frame) dereferencing a SPECIFIC task by
/// id. It retains the requested [`TaskId`], so the witness is usable ONLY alongside the
/// request it was minted for — the provenance gate in [`authorize_agent_read`] binds it
/// to [`TaskReadRequest::address`], which the identity gate has already bound to the
/// fetched task. Composed, a witness minted for one id can authorize only that one
/// task, and a by-number request can never ride a witness minted from a throwaway
/// `ById` to escape the coordinate gate.
///
/// The scope is read from the injected [`RuntimePrincipal`], not caller data; the
/// operation and its coordinate are a typed [`TaskReadAddress`]. All three (scope,
/// operation, id) are bound at mint by [`authorize_global_by_id`] — the sole
/// constructor — and the agent is taken FROM the principal, so a conversation-frame
/// principal or a by-number address cannot pick the coordinate-less arm, and a bare
/// [`AgentId`] with self-chosen labels is no usable API.
///
/// ```compile_fail
/// use silverhand_backend_core::conversation_messaging::messaging::task::{GlobalAgentRead, TaskId};
/// use silverhand_backend_core::foundation::identity::AgentId;
///
/// // The fields are private — no caller can assert "coordinate-less for this id"
/// // from bare parts; it can only come from `authorize_global_by_id`'s bind.
/// let forged = GlobalAgentRead {
///     agent: AgentId::parse("a").unwrap(),
///     requested: TaskId::parse("t").unwrap(),
/// };
/// ```
pub struct GlobalAgentRead {
    agent: AgentId,
    requested: TaskId,
}

impl GlobalAgentRead {
    /// The principal this global read is for.
    fn agent(&self) -> &AgentId {
        &self.agent
    }

    /// The task id this witness was minted for — what the REQUEST's address must name.
    fn requested(&self) -> &TaskId {
        &self.requested
    }
}

/// Mint the [`GlobalAgentRead`] witness for a coordinate-less by-id read, binding the
/// AUTHENTICATED runtime scope AND the requested id: only an agent-slot
/// [`RuntimePrincipal`] — one with no ambient conversation, so
/// [`RuntimePrincipal::source`] is `None` — dereferencing a task BY ID yields one, and
/// the id it named is retained so the read authorizes only that task. The scope is
/// read from the principal the authenticated middleware injected, NOT a caller-supplied
/// label. A conversation-frame principal (always board-scoped to its source) or a
/// by-number address (always board-resolved) mints `None`, so neither can present
/// itself as a coordinate-less read; the agent comes from the principal, never a
/// separately-supplied id.
pub fn authorize_global_by_id(
    principal: &RuntimePrincipal,
    address: &TaskReadAddress,
) -> Option<GlobalAgentRead> {
    match address {
        // Agent-slot = no ambient conversation (`source()` is `None`). A
        // conversation-frame is board-scoped to its source; a by-number address
        // always needs a board. The id is CONSUMED into the witness, not discarded.
        TaskReadAddress::ById(requested) if principal.source().is_none() => Some(GlobalAgentRead {
            agent: principal.agent().clone(),
            requested: requested.clone(),
        }),
        _ => None,
    }
}

/// Who is reading a task.
pub enum TaskReadActor {
    /// A human, gated by conversation participation alone.
    User(UserId),
    /// An agent, in one of its two read scopes.
    Agent(AgentRead),
}

/// The scope an agent reads under, which decides the coordinate gate. The agent is
/// carried by the scope itself — there is no separate agent field to desync from a
/// board authorized for someone else.
pub enum AgentRead {
    /// A coordinate-less by-id global dereference: NO board coordinate. The
    /// [`GlobalAgentRead`] witness proves this read is an authenticated agent-slot
    /// by-id operation — the arm cannot be picked from a bare agent id.
    /// Participation (and, on a workspace board, capability) is the whole authority.
    GlobalById(GlobalAgentRead),
    /// A read confined to one membership-authorized board — a frame's ambient board,
    /// a frame's explicitly-selected other workspace, or a slot's selected
    /// workspace — all minted through [`authorize_agent_board`]. The board is the
    /// coordinate LIMIT, not the read's identity: which task was asked for lives in
    /// the request's [`TaskReadAddress`], and for a by-number address that address
    /// carries its own board, which this limit must agree with.
    BoardScoped(AuthorizedAgentBoard),
}

impl AgentRead {
    /// The principal this read is for, taken from the scope so it can never be a
    /// different id than the one a board was authorized for.
    fn agent(&self) -> &AgentId {
        match self {
            AgentRead::GlobalById(global) => global.agent(),
            AgentRead::BoardScoped(board) => board.agent(),
        }
    }
}

impl TaskReadActor {
    /// The principal this read is authorized for — the subject a viewer-specific
    /// projection must bind to via [`VisibleTask::for_viewer`].
    fn viewer(&self) -> ReadViewer {
        match self {
            TaskReadActor::User(user) => ReadViewer::User(user.clone()),
            TaskReadActor::Agent(read) => ReadViewer::Agent(read.agent().clone()),
        }
    }
}

/// The two placement-participation queries a task read consults — the narrow slice
/// of the send gate's authority this rule needs, so a task read does not depend on
/// the whole write gate's `PostAuthorityReads`. One SQLite adapter can implement
/// both this port and the send gate's over the same canonical participation SQL.
pub trait TaskParticipationReads {
    fn is_agent_participant(
        &mut self,
        conversation: &ConversationId,
        agent: &AgentId,
    ) -> impl Future<Output = Result<bool, PersistenceFailure>> + Send;

    fn is_user_participant(
        &mut self,
        conversation: &ConversationId,
        user: &UserId,
    ) -> impl Future<Output = Result<bool, PersistenceFailure>> + Send;

    /// Which of `conversations` the agent participates in — the SET-shaped read
    /// [`list_visible_tasks`] gates a whole candidate set through in ONE round-trip, so a
    /// list's participation cost is CONSTANT in the number of candidate rows, not one
    /// query per row. The returned set is the participating SUBSET; the core re-binds each
    /// candidate's OWN conversation against it ([`AuthorizedTaskBoards`]-style), so a
    /// conversation the adapter never returns is simply not admitted — the SQL `IN` is a
    /// coordinate narrowing, never a witness. This is the SAME participation predicate as
    /// [`Self::is_agent_participant`] batched over a set, never a second rule (the one
    /// adapter answers the singular method as a set of one).
    fn agent_participating_conversations(
        &mut self,
        agent: &AgentId,
        conversations: &[ConversationId],
    ) -> impl Future<Output = Result<HashSet<ConversationId>, PersistenceFailure>> + Send;
}

/// Whether an agent holds `read_tasks` on a workspace — the parity of
/// `canAgentUseCapability(agent, workspaceId, 'read_tasks')`, INCLUDING its
/// agent-home owner arm. Consulted ONLY for a workspace board; a direct-pair board
/// has no capability axis. The same `owner_agent_id OR workspace_agent_capabilities`
/// shape the send gate's `SendCapability` already answers for `send_messages`, with
/// the token swapped.
pub trait TaskReadCapability {
    fn may_read_tasks(
        &mut self,
        agent: &AgentId,
        workspace: &WorkspaceId,
    ) -> impl Future<Output = Result<bool, PersistenceFailure>> + Send;

    /// Which of `workspaces` the agent may read tasks on — the SET-shaped read
    /// [`authorize_task_list_boards`] folds a whole distinct-board set through in ONE
    /// round-trip, so a list's capability cost is CONSTANT in the number of boards, not
    /// one query per board. The returned set is the admitted SUBSET; the core re-binds
    /// each workspace board against it, so a workspace the adapter never returns is simply
    /// not admitted — never trusted from the query shape. This is the SAME
    /// `owner_agent_id OR workspace_agent_capabilities` predicate as [`Self::may_read_tasks`]
    /// batched over a set, never a second capability rule (the one adapter answers the
    /// singular method as a set of one).
    fn readable_task_workspaces(
        &mut self,
        agent: &AgentId,
        workspaces: &[WorkspaceId],
    ) -> impl Future<Output = Result<HashSet<WorkspaceId>, PersistenceFailure>> + Send;
}

/// The principal a read was authorized for, retained on the success fact so a
/// viewer-specific projection binds to it before projecting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReadViewer {
    User(UserId),
    Agent(AgentId),
}

/// A read-authorized task, retaining the [`ReadViewer`] it was authorized for.
///
/// The fields are private and it is minted ONLY by this module's read authorities —
/// [`read_visible_task_on`] over one requested coordinate and [`list_visible_tasks`] over a
/// fetched candidate set — so holding one is proof the gates passed AND that the subject
/// is known. There is NO
/// unconditional accessor to the bare [`TaskAggregate`]: it is reachable only
/// through [`Self::for_viewer`], which requires presenting — and matching — the
/// authorized viewer. A viewer-specific projection therefore cannot degrade the
/// fact to a raw aggregate and pair it with another viewer.
///
/// The two gates below pin BOTH degradation shapes independently — a borrowing
/// `task(&self) -> &TaskAggregate` and a consuming `into_task(self) ->
/// TaskAggregate`. Re-adding EITHER makes its snippet compile, turning that doctest
/// RED; pinning only one would let the other reappear unnoticed.
///
/// ```compile_fail
/// // Borrowing degradation exit: a bare `&TaskAggregate` with no viewer presented.
/// use silverhand_backend_core::conversation_messaging::messaging::task::{TaskAggregate, VisibleTask};
///
/// fn borrow_bare(visible: &VisibleTask) -> &TaskAggregate {
///     visible.task()
/// }
/// ```
///
/// ```compile_fail
/// // Consuming degradation exit: an owned `TaskAggregate` with no viewer presented.
/// use silverhand_backend_core::conversation_messaging::messaging::task::{TaskAggregate, VisibleTask};
///
/// fn consume_bare(visible: VisibleTask) -> TaskAggregate {
///     visible.into_task()
/// }
/// ```
pub struct VisibleTask {
    viewer: ReadViewer,
    task: TaskAggregate,
}

impl VisibleTask {
    /// The principal this read was authorized for.
    pub fn viewer(&self) -> &ReadViewer {
        &self.viewer
    }

    /// The task, yielded ONLY when `viewer` is the principal the read was authorized
    /// for; any other viewer gets `None`. This is the SOLE accessor to the
    /// aggregate, so an A-authorized task cannot be projected into B's
    /// viewer-resolved detail or navigation: a projector must present its own viewer
    /// and it must match. A genuinely viewer-independent projection should consume
    /// the witness through a narrow operation-specific method rather than a bare
    /// degradation exit.
    pub fn for_viewer(&self, viewer: &ReadViewer) -> Option<&TaskAggregate> {
        (self.viewer == *viewer).then_some(&self.task)
    }

    pub(crate) fn into_parts(self) -> (ReadViewer, TaskAggregate) {
        (self.viewer, self.task)
    }
}

/// Returns the read-authorized `task` iff `request`'s actor may see it, otherwise
/// `None`. A `PersistenceFailure` is a safe-500 from an authority, never a denial.
///
/// `task` must be the row the lookup produced for `request.address` — the identity gate
/// below refuses (as corruption) any other, for EVERY actor, before consulting a single
/// authority.
/// Participation and capability reads share one connection-bound authority.
pub async fn read_visible_task_on<A>(
    request: &TaskReadRequest,
    task: TaskAggregate,
    authority: &mut A,
) -> Result<Option<VisibleTask>, PersistenceFailure>
where
    A: TaskParticipationReads + TaskReadCapability,
{
    if !request.address.names(&task) {
        return Err(PersistenceFailure::from_source(AddressIdentityViolation));
    }
    let authorized = match &request.actor {
        TaskReadActor::User(user) => {
            authority
                .is_user_participant(task.conversation_id(), user)
                .await?
        }
        TaskReadActor::Agent(read) => {
            authorize_agent_read_on(read, &request.address, &task, authority).await?
        }
    };
    Ok(authorized.then_some(VisibleTask {
        viewer: request.actor.viewer(),
        task,
    }))
}

/// After the actor-independent identity gate: witness provenance, board coordinate,
/// participation, then workspace capability. The scope supplies the subject. A
/// provenance mismatch is corruption; other denials short-circuit later authorities.
async fn authorize_agent_read_on<A>(
    read: &AgentRead,
    address: &TaskReadAddress,
    task: &TaskAggregate,
    authority: &mut A,
) -> Result<bool, PersistenceFailure>
where
    A: TaskParticipationReads + TaskReadCapability,
{
    let agent = read.agent();
    if let AgentRead::GlobalById(global) = read
        && !matches!(address, TaskReadAddress::ById(id) if id == global.requested())
    {
        return Err(PersistenceFailure::from_source(AddressProvenanceViolation));
    }
    if let AgentRead::BoardScoped(board) = read
        && task.board() != board.board()
    {
        return Ok(false);
    }
    if !authority
        .is_agent_participant(task.conversation_id(), agent)
        .await?
    {
        return Ok(false);
    }
    if let Some(workspace) = task.board().workspace_id()
        && !authority.may_read_tasks(agent, workspace).await?
    {
        return Ok(false);
    }
    Ok(true)
}

// ============================================================================
// Board list read model
// ============================================================================
//
// The list is the plural sibling of [`read_visible_task_on`]: instead of one
// requested coordinate it walks a fetched CANDIDATE SET and keeps the rows a
// viewer may see, in the arm's order, trimmed to a limit. It composes the SAME
// ports and mints the SAME [`VisibleTask`] witness, so a listed task is exactly
// as viewer-bound as a point-read one.
//
// The single-task authority ran five ANDed gates; the list splits them by WHEN
// they can be settled, so the per-row loop only asks what genuinely varies per
// row:
//
//   * IDENTITY / WITNESS PROVENANCE have no analogue — a list has no single
//     requested coordinate to bind a row against, and there is no
//     coordinate-less witness to place.
//   * COORDINATE + CAPABILITY are settled ONCE, up front, into the agent scope:
//     [`authorize_task_list_boards`] takes the coordinate+provenance-bound
//     [`AuthorizedAgentBoard`] witnesses [`authorize_agent_board`] mints and
//     folds the `read_tasks` capability into them, minting an
//     [`AuthorizedTaskBoards`] set. Holding that set is proof every board in it
//     passed BOTH gates — so a list entry point cannot degrade to a caller
//     passing raw `Vec<BoardKey>` and asserting "already authorized". Capability
//     is per-(agent, workspace), constant across the rows on a board, so folding
//     it into the set answers it once rather than per row.
//   * PARTICIPATION is the one gate that genuinely varies per row (it is keyed by
//     the row's OWN conversation), so it stays in the per-row loop, through the
//     same [`TaskParticipationReads`] port — never re-encoded as list SQL.
//
// The board COORDINATE also gets re-bound per row against the candidate's OWN
// decoded board, never trusting the fetch's `WHERE`: the adapter is free to
// pre-filter by board for efficiency, but a row only survives if the board it
// actually decoded to is in the authorized set. There is no human list arm — the
// unbounded human `GET /api/tasks` was retired, so a board list is always an
// agent's authorized board set and the per-row gate is coordinate ∧ participation.

/// The closed set of DISTINCT boards a board-scoped agent list is confined to, each
/// already coordinate+provenance-authorized ([`authorize_agent_board`]) AND
/// capability-cleared (`read_tasks` for a workspace board; a direct-pair board has no
/// capability axis), bound to the single agent they were all authorized for.
///
/// The fields are private and the sole minter is [`authorize_task_list_boards`], which
/// dedups by [`BoardKey`], so holding one is proof the coordinate, provenance, and
/// capability gates are settled for every board it carries AND that no board appears
/// twice — a list entry point that consumes this cannot be handed a raw `Vec<BoardKey>`
/// with an unbacked "already authorized" claim, nor can a repeated authorization skew the
/// single-vs-multi-board ordering arm. The agent lives inside the set, so the
/// participation gate below asks about the agent the boards were authorized for, never a
/// separately-supplied id.
pub struct AuthorizedTaskBoards {
    agent: AgentId,
    boards: Vec<BoardKey>,
}

impl AuthorizedTaskBoards {
    /// The agent every board in the set was authorized for.
    fn agent(&self) -> &AgentId {
        &self.agent
    }

    /// Whether `board` — a candidate row's OWN decoded board — is one this list is
    /// confined to. The per-row coordinate re-bind, never trusting the fetch's `WHERE`.
    fn contains(&self, board: &BoardKey) -> bool {
        self.boards.iter().any(|authorized| authorized == board)
    }

    /// A union over more than one DISTINCT board — the arm the ordering policy treats
    /// differently (see [`ListOrder`]). The minter deduped by [`BoardKey`], so a board
    /// authorized more than once cannot inflate this count past the boards actually in
    /// scope.
    fn is_multi_board(&self) -> bool {
        self.boards.len() > 1
    }

    /// The ordering the surviving rows are sorted by: a multi-board union orders by
    /// recency (board-local numbers are not comparable ACROSS boards), a single board
    /// by its board-local number — both with the `id DESC` total-order tiebreak.
    fn order(&self) -> ListOrder {
        if self.is_multi_board() {
            ListOrder::CreatedAtDesc
        } else {
            ListOrder::NumberDesc
        }
    }

    /// The viewer every surviving row is minted as — always the agent the boards were
    /// authorized for, the subject a projector later presents to read the row back.
    fn viewer(&self) -> ReadViewer {
        ReadViewer::Agent(self.agent.clone())
    }

    /// The board-key set the server projects into the candidate fetch's `WHERE board_key
    /// IN (...)` narrowing — a pure EFFICIENCY coordinate that carries NO authorization
    /// meaning on its own. The authority stays the per-row [`Self::contains`] re-bind in
    /// [`list_visible_tasks`]: a fetch that ignored, widened, or hand-mutated this filter
    /// still cannot surface a row whose OWN decoded board is outside the set. It is
    /// exported only because the set's fields are private (holding the witness IS the
    /// type-fact of authorization), so a server bridge has no other way to read the
    /// coordinate it must narrow by — this is the SOLE controlled read, deliberately a
    /// coordinate export and not a general "these boards are authorized" accessor.
    pub fn fetch_coordinate(&self) -> Vec<BoardKey> {
        self.boards.clone()
    }
}

/// The typed corruption raised when a board-scoped list scope is assembled from a board
/// authorized for a DIFFERENT agent than the one the set is being minted for. Every
/// witness in the set must name the same subject; a mismatch is a wired-wrong call site
/// — a safe-500, never a caller denial.
#[derive(Debug, thiserror::Error)]
#[error("a board-scoped list scope was built from a board authorized for a different agent")]
struct ListScopeSubjectViolation;

/// Mint the [`AuthorizedTaskBoards`] a board-scoped agent list is confined to. It runs
/// in two passes so the "set" its name promises actually holds:
///
///  1. Bind every witness's subject to `agent` (a foreign witness is a safe-500) and
///     collapse to DISTINCT boards. A board authorized twice for the same agent is the
///     SAME authorization, so a duplicate must not change the product result — neither
///     the row set nor the board COUNT the ordering arm reads (a duplicated single board
///     must stay `task_number DESC`, never flip to the multi-board `created_at DESC`).
///  2. Fold the `read_tasks` capability ONCE per distinct board: a workspace board enters
///     the scope only when the agent holds the capability there; a direct-pair board has
///     no capability axis and is admitted on its participation alone (settled per-row
///     below). Deduping first means a repeated input board never re-asks — and so cannot
///     amplify — the external capability read.
///
/// The resulting set may be empty (no authorized board), which lists as nothing. This is
/// the ONLY constructor of [`AuthorizedTaskBoards`], so the coordinate, provenance, and
/// capability gates — and the distinct-board invariant the ordering arm depends on — are
/// a TYPE FACT of the scope a list consumes, not a caller assertion.
pub async fn authorize_task_list_boards<C>(
    agent: AgentId,
    boards: Vec<AuthorizedAgentBoard>,
    capability: &mut C,
) -> Result<AuthorizedTaskBoards, PersistenceFailure>
where
    C: TaskReadCapability,
{
    // Pass 1: subject-bind, then collapse to distinct boards.
    let mut distinct: Vec<BoardKey> = Vec::with_capacity(boards.len());
    for board in boards {
        if board.agent() != &agent {
            return Err(PersistenceFailure::from_source(ListScopeSubjectViolation));
        }
        if !distinct.contains(board.board()) {
            distinct.push(board.board().clone());
        }
    }

    // Pass 2: fold the `read_tasks` capability over the WHOLE distinct workspace-board set
    // in ONE round-trip, then re-bind each board against the admitted subset. A direct-pair
    // board has no capability axis (always admitted); only workspace boards are asked, and
    // an empty workspace set skips the read entirely. The returned set is a coordinate
    // narrowing, never a witness: a board it omits is dropped, and a board it could not
    // have covered can never appear here (every entry came from Pass 1's authorized
    // witnesses). Cost is constant in the number of boards.
    let workspaces: Vec<WorkspaceId> = distinct
        .iter()
        .filter_map(BoardKey::workspace_id)
        .cloned()
        .collect();
    let readable = if workspaces.is_empty() {
        HashSet::new()
    } else {
        capability
            .readable_task_workspaces(&agent, &workspaces)
            .await?
    };
    let authorized: Vec<BoardKey> = distinct
        .into_iter()
        .filter(|board| match board.workspace_id() {
            Some(workspace) => readable.contains(workspace),
            None => true,
        })
        .collect();
    Ok(AuthorizedTaskBoards {
        agent,
        boards: authorized,
    })
}

/// The display strings a board-list projection needs that are NOT on the committed
/// [`TaskAggregate`] — the joined channel title and the assignee/creator display names.
/// They are read in the SAME joined select as the candidate (never an N+1 second read)
/// and ride UNTOUCHED through core visibility, ordering, and trimming, so the server
/// projects the surviving [`VisibleTaskListRow`] straight to its DTO without a
/// split-into-ids-then-rejoin seam it does not own.
///
/// Every name is the joined `display_name` when the actor join matched, or `None` when it
/// did not (a deleted/unlinked actor) — the projector degrades a `None` to the id.
/// `channel_name` is `Some` only for a workspace board (the conversation title); a
/// direct-pair board projects `None` (its sentinel storage title never enters a read
/// model), so a workspace board whose `channel_name` is `None` is a corrupt row the
/// projector rejects rather than inventing a place.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskDisplaySidecars {
    /// The conversation title for a workspace board; `None` for a direct-pair board.
    pub channel_name: Option<String>,
    /// The assignee agent's joined display name, if the assignee is an agent and present.
    pub assignee_agent_name: Option<String>,
    /// The assignee user's joined display name, if the assignee is a user and present.
    pub assignee_user_name: Option<String>,
    /// The creator agent's joined display name, if the creator is an agent and present.
    pub creator_agent_name: Option<String>,
    /// The creator user's joined display name, if the creator is a user and present.
    pub creator_user_name: Option<String>,
}

/// One row a board-list fetch produced: the strict-decoded [`TaskAggregate`] the core
/// authorizes, sorts, and trims, PLUS the [`TaskDisplaySidecars`] the same joined select
/// read for it. The aggregate stays the sole authority axis (board, conversation,
/// numbers); the sidecar is inert display data the core never reads, only carries.
pub struct TaskListCandidate {
    task: TaskAggregate,
    display: TaskDisplaySidecars,
}

impl TaskListCandidate {
    /// Pair a decoded aggregate with the display sidecars its joined row read — the only
    /// constructor, so a candidate is always an aggregate the fetch strict-decoded plus
    /// the sidecar from the SAME row, never a hand-assembled splice.
    pub fn new(task: TaskAggregate, display: TaskDisplaySidecars) -> Self {
        Self { task, display }
    }

    /// The strict-decoded aggregate this candidate carries — for the fetch producer's own
    /// verification and the visibility pipeline. This is NOT the server's projection path:
    /// a candidate is pre-authorization, so the DTO-projection input [`TaskSummarySource`]
    /// is reachable only from a viewer-bound [`VisibleTaskListRow`], never from here.
    pub fn task(&self) -> &TaskAggregate {
        &self.task
    }

    /// The display sidecars read for this candidate in the same joined row — for the fetch
    /// producer's verification, on the same pre-authorization terms as [`Self::task`].
    pub fn display(&self) -> &TaskDisplaySidecars {
        &self.display
    }
}

/// The per-arm ordering the board list is sorted by, a TOTAL order in every arm — the
/// `id DESC` tiebreak removes the equal-key ambiguity a `(number)` or `(created_at)`
/// key alone leaves. This is deliberately STRONGER than (a refinement of, never in
/// conflict with) the TS list's ordering: the Rust cutover pins a deterministic total
/// order so a page boundary can never split two rows that share the leading key.
enum ListOrder {
    /// `task_number DESC, id DESC` — a single board (or a human reading one board's
    /// worth of tasks): board-local numbers are meaningful and monotonic.
    NumberDesc,
    /// `created_at DESC, id DESC` — a multi-board agent union, where board-local numbers
    /// are not comparable ACROSS boards, so recency is the only shared axis.
    CreatedAtDesc,
}

/// A read-authorized board-list row: the surviving [`TaskAggregate`], the
/// [`TaskDisplaySidecars`] that rode through with it, and the [`ReadViewer`] the list was
/// authorized for. The plural analogue of [`VisibleTask`] — holding one is proof the
/// coordinate, capability, and participation gates passed for this row and that it is
/// bound to the viewer a projector must present it as. The aggregate and sidecar are
/// reachable ONLY together through [`Self::summary`], so a projection reads the whole row
/// the core returned rather than re-splitting it into an id map to rejoin.
pub struct VisibleTaskListRow {
    viewer: ReadViewer,
    task: TaskAggregate,
    display: TaskDisplaySidecars,
}

impl VisibleTaskListRow {
    /// The viewer this row was authorized for — the subject a projector presents it as.
    pub fn viewer(&self) -> &ReadViewer {
        &self.viewer
    }

    /// The aggregate and its display sidecars, borrowed together for projection. There is
    /// no accessor that yields the aggregate WITHOUT the sidecar (or vice versa): the
    /// server projects this bundle straight to its summary DTO, so the two halves the one
    /// joined row produced can never drift apart into a rejoin seam.
    pub fn summary(&self) -> TaskSummarySource<'_> {
        TaskSummarySource {
            task: &self.task,
            display: &self.display,
        }
    }
}

/// The borrowed projection input a [`VisibleTaskListRow`] hands a server DTO mapper: the
/// committed aggregate and the display sidecars that rode with it, together. A transient
/// borrow bundle, not a stored value — it exists only so the projection reads both halves
/// of the one joined row at once.
pub struct TaskSummarySource<'a> {
    /// The committed task — the authority for id, number, board, status, timestamps.
    pub task: &'a TaskAggregate,
    /// The display sidecars the joined row read for this task.
    pub display: &'a TaskDisplaySidecars,
}

/// Order two candidates under `order` — a total order, DESC on the leading key with an
/// `id DESC` tiebreak, so `b` sorts before `a` when it ranks higher.
fn compare_candidates(order: &ListOrder, a: &TaskAggregate, b: &TaskAggregate) -> Ordering {
    let leading = match order {
        ListOrder::NumberDesc => b.task_number().get().cmp(&a.task_number().get()),
        ListOrder::CreatedAtDesc => b.created_at().cmp(&a.created_at()),
    };
    leading.then_with(|| b.id().as_str().cmp(a.id().as_str()))
}

/// The board list authority: from the DECODED `candidates` a fetch produced, keep the
/// rows this authorized agent may see, in the arm's order, trimmed to `limit`, each minted
/// as a viewer-bound [`VisibleTaskListRow`]. A `PersistenceFailure` is a safe-500 from an
/// authority, never a denial.
///
/// The scope is always an agent's authorized board set: the unbounded human list arm was
/// retired (the human board is a separate cursor read model), so a list has exactly one
/// authority shape and the per-row gate asks only what varies per row. Each candidate's
/// [`TaskAggregate`] must already be strict-decoded — including any row the fetch read to
/// decide ordering or a page boundary — so a corrupt row is a safe-500 BEFORE the trim,
/// never a silently-dropped one; they may arrive in any order (this sorts). The fetch may
/// pre-filter by board and the other coordinate filters for efficiency, but this re-binds
/// each row's OWN decoded board against the authorized set rather than trusting that
/// `WHERE`. Participation is settled here per-row through the port, so the fetch carries NO
/// participation predicate and NO SQL `LIMIT` (participation removes rows a SQL limit would
/// have miscounted); `limit` is applied to the VISIBLE rows here. A `limit` of `None` keeps
/// every visible row (a caller maps a non-positive TS limit to `None`). The display
/// sidecar each candidate carries rides through untouched onto the surviving row.
pub async fn list_visible_tasks<P>(
    boards: &AuthorizedTaskBoards,
    mut candidates: Vec<TaskListCandidate>,
    participation: &mut P,
    limit: Option<usize>,
) -> Result<Vec<VisibleTaskListRow>, PersistenceFailure>
where
    P: TaskParticipationReads,
{
    let order = boards.order();
    candidates.sort_by(|a, b| compare_candidates(&order, &a.task, &b.task));

    // COORDINATE re-bind first: only a candidate whose OWN decoded board is in the
    // authorized set is a participation question at all (an off-board row consults no
    // authority and is dropped). Gate the PARTICIPATION of that whole on-board set in ONE
    // round-trip — cost constant in the number of candidates, never one query per row —
    // then bind each row against the participating subset. Capability was folded into the
    // board set at mint; the returned conversation set is a coordinate narrowing, never a
    // witness (a conversation it omits is dropped, one it never covered cannot appear).
    let conversations: Vec<ConversationId> = {
        let mut distinct = HashSet::new();
        for candidate in &candidates {
            if boards.contains(candidate.task.board()) {
                distinct.insert(candidate.task.conversation_id().clone());
            }
        }
        distinct.into_iter().collect()
    };
    let participating = if conversations.is_empty() {
        HashSet::new()
    } else {
        participation
            .agent_participating_conversations(boards.agent(), &conversations)
            .await?
    };

    let viewer = boards.viewer();
    let mut visible = Vec::new();
    for candidate in candidates {
        if let Some(limit) = limit
            && visible.len() >= limit
        {
            break;
        }

        // Re-bind each row against BOTH the authorized board set and the participating
        // conversation set the single read returned — the limit trims VISIBLE rows.
        let admitted = boards.contains(candidate.task.board())
            && participating.contains(candidate.task.conversation_id());

        if admitted {
            visible.push(VisibleTaskListRow {
                viewer: viewer.clone(),
                task: candidate.task,
                display: candidate.display,
            });
        }
    }
    Ok(visible)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_messaging::messaging::task::TaskRow;
    use crate::foundation::identity::DaemonInstanceId;
    use crate::host_runtime::runtime_credential::{RuntimeCustody, RuntimeGeneration};

    /// A `todo` task on `ws:workspace_1` in `conv_1`.
    fn workspace_task() -> TaskAggregate {
        task_on("ws:workspace_1", Some("workspace_1"))
    }

    /// A `todo` task on the direct-pair board `pair:pair_1` in `conv_1`.
    fn pair_task() -> TaskAggregate {
        task_on("pair:pair_1", None)
    }

    fn task_on(board_key: &str, workspace_id: Option<&str>) -> TaskAggregate {
        task_on_full("task_1", 1, board_key, workspace_id)
    }

    /// A fixture task with an EXPLICIT id and board-local number — for the identity
    /// gate falsifiers, where the fetched task must differ from the requested
    /// coordinate.
    fn task_on_full(
        id: &str,
        task_number: i64,
        board_key: &str,
        workspace_id: Option<&str>,
    ) -> TaskAggregate {
        TaskAggregate::decode(TaskRow {
            id: id.into(),
            task_number,
            conversation_id: "conv_1".into(),
            thread_root_id: None,
            board_key: board_key.into(),
            workspace_id: workspace_id.map(Into::into),
            title: "a title".into(),
            content: String::new(),
            status: "todo".into(),
            assignee_agent_id: None,
            assignee_user_id: None,
            creator_kind: "system".into(),
            creator_agent_id: None,
            creator_user_id: None,
            source_message_id: None,
            created_event_message_id: None,
            last_editor_kind: None,
            last_editor_id: None,
            last_edited_at: None,
            created_at: 100,
            closed_at: None,
        })
        .expect("the fixture row is a valid committed fact")
    }

    fn workspace_board(id: &str) -> BoardKey {
        BoardKey::Workspace(WorkspaceId::parse(id).unwrap())
    }

    fn agent_id(raw: &str) -> AgentId {
        AgentId::parse(raw).unwrap()
    }

    /// A durable custody stamp for a constructed test principal — its value is
    /// irrelevant to the scope discrimination under test.
    fn custody() -> RuntimeCustody {
        RuntimeCustody::new(
            RuntimeGeneration::new(0).unwrap(),
            DaemonInstanceId::parse("daemon-1").unwrap(),
        )
    }

    /// A REAL authenticated agent-slot principal — no ambient conversation.
    fn agent_slot(agent: &str) -> RuntimePrincipal {
        RuntimePrincipal::AgentSlot {
            agent: agent_id(agent),
            custody: custody(),
        }
    }

    /// A REAL authenticated conversation-frame principal, bound to its source.
    fn conversation_frame(agent: &str, source: &str) -> RuntimePrincipal {
        RuntimePrincipal::ConversationSlot {
            agent: agent_id(agent),
            source: ConversationId::parse(source).unwrap(),
            custody: custody(),
        }
    }

    /// A REAL by-id address fact carrying an opaque id — names `task_1`, the id every
    /// fixture task carries unless told otherwise.
    fn by_id() -> TaskReadAddress {
        by_id_of("task_1")
    }

    fn by_id_of(id: &str) -> TaskReadAddress {
        TaskReadAddress::ById(TaskId::parse(id).unwrap())
    }

    /// A REAL by-number address fact: a board-local number AND the board it is local
    /// to. Names the fixture task's own coordinate (`workspace_1`, number 1).
    fn by_number() -> TaskReadAddress {
        by_number_on("workspace_1", 1)
    }

    fn by_number_on(workspace: &str, number: i64) -> TaskReadAddress {
        TaskReadAddress::ByNumber {
            board: workspace_board(workspace),
            number: TaskNumber::new(number).unwrap(),
        }
    }

    /// A board-scoped actor for `agent` on `board`. Tests mint the board witness
    /// through the module-private constructor (a child module sees it); external crates
    /// cannot — see the `compile_fail` gate on [`AuthorizedAgentBoard`].
    fn board_scoped(agent: &str, board: BoardKey) -> TaskReadActor {
        TaskReadActor::Agent(AgentRead::BoardScoped(AuthorizedAgentBoard(
            agent_id(agent),
            board,
        )))
    }

    /// A global by-id actor for `agent`, minted through the real provenance bind (an
    /// authenticated agent-slot principal doing a by-id operation) — never assembled
    /// from a bare id, which no arm allows.
    fn global_by_id(agent: &str) -> TaskReadActor {
        global_by_id_for(agent, &by_id())
    }

    /// A global actor minted for an EXPLICIT address — for the provenance falsifiers,
    /// where the witness is deliberately minted for one request and then presented with
    /// another.
    fn global_by_id_for(agent: &str, address: &TaskReadAddress) -> TaskReadActor {
        TaskReadActor::Agent(AgentRead::GlobalById(
            authorize_global_by_id(&agent_slot(agent), address)
                .expect("an agent-slot by-id read mints a global witness"),
        ))
    }

    fn human() -> TaskReadActor {
        TaskReadActor::User(UserId::parse("user_1").unwrap())
    }

    fn request(address: TaskReadAddress, actor: TaskReadActor) -> TaskReadRequest {
        TaskReadRequest { address, actor }
    }

    /// Participation with fixed verdicts for the human and for ANY agent.
    struct Participation {
        agent: bool,
        user: bool,
    }

    impl TaskParticipationReads for Participation {
        async fn is_agent_participant(
            &mut self,
            _conversation: &ConversationId,
            _agent: &AgentId,
        ) -> Result<bool, PersistenceFailure> {
            Ok(self.agent)
        }

        async fn is_user_participant(
            &mut self,
            _conversation: &ConversationId,
            _user: &UserId,
        ) -> Result<bool, PersistenceFailure> {
            Ok(self.user)
        }

        async fn agent_participating_conversations(
            &mut self,
            agent: &AgentId,
            conversations: &[ConversationId],
        ) -> Result<HashSet<ConversationId>, PersistenceFailure> {
            let mut set = HashSet::new();
            for conversation in conversations {
                if self.is_agent_participant(conversation, agent).await? {
                    set.insert(conversation.clone());
                }
            }
            Ok(set)
        }
    }

    /// Participation that answers `true` ONLY for one named agent — used to prove
    /// the gate asks about the scope's own agent, not any other.
    struct ParticipationFor {
        agent: &'static str,
    }

    impl TaskParticipationReads for ParticipationFor {
        async fn is_agent_participant(
            &mut self,
            _conversation: &ConversationId,
            agent: &AgentId,
        ) -> Result<bool, PersistenceFailure> {
            Ok(agent.as_str() == self.agent)
        }

        async fn is_user_participant(
            &mut self,
            _conversation: &ConversationId,
            _user: &UserId,
        ) -> Result<bool, PersistenceFailure> {
            Ok(false)
        }

        async fn agent_participating_conversations(
            &mut self,
            agent: &AgentId,
            conversations: &[ConversationId],
        ) -> Result<HashSet<ConversationId>, PersistenceFailure> {
            let mut set = HashSet::new();
            for conversation in conversations {
                if self.is_agent_participant(conversation, agent).await? {
                    set.insert(conversation.clone());
                }
            }
            Ok(set)
        }
    }

    /// Participation that RECORDS every consultation and would GRANT if asked — so a
    /// test can prove a gate refused BEFORE any authority was consulted (`calls == 0`),
    /// not merely that the outcome was a refusal.
    struct CountingParticipation {
        calls: usize,
    }

    impl TaskParticipationReads for CountingParticipation {
        async fn is_agent_participant(
            &mut self,
            _conversation: &ConversationId,
            _agent: &AgentId,
        ) -> Result<bool, PersistenceFailure> {
            self.calls += 1;
            Ok(true)
        }

        async fn is_user_participant(
            &mut self,
            _conversation: &ConversationId,
            _user: &UserId,
        ) -> Result<bool, PersistenceFailure> {
            self.calls += 1;
            Ok(true)
        }

        async fn agent_participating_conversations(
            &mut self,
            _agent: &AgentId,
            conversations: &[ConversationId],
        ) -> Result<HashSet<ConversationId>, PersistenceFailure> {
            // ONE round-trip for the whole candidate set — the property the list's
            // set-shaped participation read guarantees; grants all, like the singular.
            self.calls += 1;
            Ok(conversations.iter().cloned().collect())
        }
    }

    struct Capability {
        grant: bool,
    }

    impl TaskReadCapability for Capability {
        async fn may_read_tasks(
            &mut self,
            _agent: &AgentId,
            _workspace: &WorkspaceId,
        ) -> Result<bool, PersistenceFailure> {
            Ok(self.grant)
        }

        async fn readable_task_workspaces(
            &mut self,
            _agent: &AgentId,
            workspaces: &[WorkspaceId],
        ) -> Result<HashSet<WorkspaceId>, PersistenceFailure> {
            Ok(if self.grant {
                workspaces.iter().cloned().collect()
            } else {
                HashSet::new()
            })
        }
    }

    /// A capability authority that panics if consulted — proves a direct-pair board
    /// never touches the capability axis.
    struct NeverCapability;

    impl TaskReadCapability for NeverCapability {
        async fn may_read_tasks(
            &mut self,
            _agent: &AgentId,
            _workspace: &WorkspaceId,
        ) -> Result<bool, PersistenceFailure> {
            unimplemented!("a direct-pair board must not consult the read_tasks capability")
        }

        async fn readable_task_workspaces(
            &mut self,
            _agent: &AgentId,
            _workspaces: &[WorkspaceId],
        ) -> Result<HashSet<WorkspaceId>, PersistenceFailure> {
            unimplemented!("a direct-pair board must not consult the read_tasks capability")
        }
    }

    /// Combines independent fixture verdicts into the same authority shape the real
    /// snapshot adapter supplies. Policy remains entirely in the production owner.
    struct TestAuthority<'a, P, C> {
        participation: &'a mut P,
        capability: &'a mut C,
    }

    impl<P: TaskParticipationReads + Send, C: Send> TaskParticipationReads for TestAuthority<'_, P, C> {
        async fn is_agent_participant(
            &mut self,
            conversation: &ConversationId,
            agent: &AgentId,
        ) -> Result<bool, PersistenceFailure> {
            self.participation
                .is_agent_participant(conversation, agent)
                .await
        }

        async fn is_user_participant(
            &mut self,
            conversation: &ConversationId,
            user: &UserId,
        ) -> Result<bool, PersistenceFailure> {
            self.participation
                .is_user_participant(conversation, user)
                .await
        }

        async fn agent_participating_conversations(
            &mut self,
            agent: &AgentId,
            conversations: &[ConversationId],
        ) -> Result<HashSet<ConversationId>, PersistenceFailure> {
            self.participation
                .agent_participating_conversations(agent, conversations)
                .await
        }
    }

    impl<P: Send, C: TaskReadCapability + Send> TaskReadCapability for TestAuthority<'_, P, C> {
        async fn may_read_tasks(
            &mut self,
            agent: &AgentId,
            workspace: &WorkspaceId,
        ) -> Result<bool, PersistenceFailure> {
            self.capability.may_read_tasks(agent, workspace).await
        }

        async fn readable_task_workspaces(
            &mut self,
            agent: &AgentId,
            workspaces: &[WorkspaceId],
        ) -> Result<HashSet<WorkspaceId>, PersistenceFailure> {
            self.capability
                .readable_task_workspaces(agent, workspaces)
                .await
        }
    }

    /// Runs a read whose address names the fixture task by its own id, so the identity
    /// gate passes and the test's own gate is the only one in question.
    async fn visible(
        actor: TaskReadActor,
        task: TaskAggregate,
        agent: bool,
        user: bool,
        grant: bool,
    ) -> bool {
        visible_at(by_id(), actor, task, agent, user, grant).await
    }

    async fn visible_at(
        address: TaskReadAddress,
        actor: TaskReadActor,
        task: TaskAggregate,
        agent: bool,
        user: bool,
        grant: bool,
    ) -> bool {
        let mut participation = Participation { agent, user };
        let mut capability = Capability { grant };
        read_visible_task_on(
            &request(address, actor),
            task,
            &mut TestAuthority {
                participation: &mut participation,
                capability: &mut capability,
            },
        )
        .await
        .expect("the fake authorities never fail")
        .is_some()
    }

    /// Runs a read expected to be REFUSED as corruption before any authority is
    /// consulted, returning the failure and the number of participation calls.
    async fn refused_before_authorities(
        address: TaskReadAddress,
        actor: TaskReadActor,
        task: TaskAggregate,
    ) -> (PersistenceFailure, usize) {
        let mut participation = CountingParticipation { calls: 0 };
        let mut capability = Capability { grant: true };
        let failure = read_visible_task_on(
            &request(address, actor),
            task,
            &mut TestAuthority {
                participation: &mut participation,
                capability: &mut capability,
            },
        )
        .await
        .err()
        .expect("a read answered off its requested coordinate is a safe-500");
        (failure, participation.calls)
    }

    // ---- Human arm -------------------------------------------------------------

    #[tokio::test]
    async fn a_participating_human_sees_the_task() {
        assert!(visible(human(), workspace_task(), false, true, false).await);
    }

    #[tokio::test]
    async fn a_non_participating_human_is_denied() {
        assert!(!visible(human(), workspace_task(), false, false, false).await);
    }

    // ---- Coordinate / participation / capability gates -------------------------

    #[tokio::test]
    async fn a_board_scoped_agent_on_its_board_sees_the_task() {
        let actor = board_scoped("agent_1", workspace_board("workspace_1"));
        assert!(visible(actor, workspace_task(), true, false, true).await);
    }

    /// COORDINATE falsified in isolation: participates AND holds capability, yet the
    /// task sits on a board the read was not scoped to.
    #[tokio::test]
    async fn a_board_scoped_agent_off_its_board_is_denied() {
        let actor = board_scoped("agent_1", workspace_board("workspace_2"));
        assert!(!visible(actor, workspace_task(), true, false, true).await);
    }

    /// PARTICIPATION falsified in isolation: board matches and capability is held,
    /// yet the agent does not participate in the task's conversation.
    #[tokio::test]
    async fn a_non_participating_board_scoped_agent_is_denied() {
        let actor = board_scoped("agent_1", workspace_board("workspace_1"));
        assert!(!visible(actor, workspace_task(), false, false, true).await);
    }

    /// The by-id global arm has NO coordinate gate: it reaches a task on any board.
    #[tokio::test]
    async fn a_global_by_id_agent_sees_a_task_on_any_board() {
        assert!(visible(global_by_id("agent_1"), workspace_task(), true, false, true).await);
    }

    /// Participation is NEVER bypassed for the global arm: capability without
    /// participation still denies.
    #[tokio::test]
    async fn a_global_by_id_agent_without_participation_is_denied() {
        assert!(
            !visible(
                global_by_id("agent_1"),
                workspace_task(),
                false,
                false,
                true
            )
            .await
        );
    }

    /// CAPABILITY falsified in isolation on a workspace board, on BOTH agent arms.
    #[tokio::test]
    async fn an_agent_without_read_tasks_is_denied_on_a_workspace_board() {
        let scoped = board_scoped("agent_1", workspace_board("workspace_1"));
        assert!(!visible(scoped, workspace_task(), true, false, false).await);
        assert!(
            !visible(
                global_by_id("agent_1"),
                workspace_task(),
                true,
                false,
                false
            )
            .await
        );
    }

    /// A direct-pair board carries no capability axis: a participant sees a
    /// pair-board task without the capability authority being consulted (it panics
    /// if reached).
    #[tokio::test]
    async fn a_direct_pair_task_needs_no_capability() {
        let mut participation = Participation {
            agent: true,
            user: false,
        };
        let mut capability = NeverCapability;
        let seen = read_visible_task_on(
            &request(by_id(), global_by_id("agent_1")),
            pair_task(),
            &mut TestAuthority {
                participation: &mut participation,
                capability: &mut capability,
            },
        )
        .await
        .expect("participation never fails")
        .is_some();
        assert!(seen);
    }

    // ---- P0-2: the gate asks about the witness's own agent ---------------------

    /// A board minted for `agent_1` drives participation for `agent_1`: with a
    /// participation authority that only recognizes `agent_1`, the read is allowed;
    /// there is no separate agent field to point at anyone else. (Constructing an
    /// actor that pairs one agent's witness with another principal is impossible —
    /// the agent lives inside the scope.)
    #[tokio::test]
    async fn a_board_scoped_read_is_evaluated_for_the_witness_agent() {
        let actor = board_scoped("agent_1", workspace_board("workspace_1"));
        let mut participation = ParticipationFor { agent: "agent_1" };
        let mut capability = Capability { grant: true };
        assert!(
            read_visible_task_on(
                &request(by_id(), actor),
                workspace_task(),
                &mut TestAuthority {
                    participation: &mut participation,
                    capability: &mut capability,
                },
            )
            .await
            .unwrap()
            .is_some()
        );

        let mismatched = board_scoped("agent_2", workspace_board("workspace_1"));
        let mut participation = ParticipationFor { agent: "agent_1" };
        let mut capability = Capability { grant: true };
        assert!(
            read_visible_task_on(
                &request(by_id(), mismatched),
                workspace_task(),
                &mut TestAuthority {
                    participation: &mut participation,
                    capability: &mut capability,
                },
            )
            .await
            .unwrap()
            .is_none()
        );
    }

    // ---- P0: the coordinate-less global arm is a provenance-bound witness -------

    /// Positive control: a REAL authenticated agent-slot principal doing a by-id
    /// operation mints a global read whose agent is taken FROM the principal — not a
    /// separately-supplied id.
    #[test]
    fn an_agent_slot_by_id_mints_a_global_read_for_the_authenticated_agent() {
        let read = authorize_global_by_id(&agent_slot("agent_1"), &by_id())
            .expect("an agent-slot by-id read mints a global witness");
        assert_eq!(read.agent(), &agent_id("agent_1"));
    }

    /// Falsifier from a REAL conversation-frame principal: it is ALWAYS board-scoped
    /// to its source, so it cannot mint the coordinate-less arm even by id. Deleting
    /// the `source().is_none()` bind would let a frame principal skip the coordinate
    /// gate, flipping this to `Some` — RED.
    #[test]
    fn a_conversation_frame_principal_cannot_mint_a_global_read() {
        assert!(
            authorize_global_by_id(&conversation_frame("agent_1", "conv_1"), &by_id()).is_none()
        );
    }

    /// Falsifier from a REAL by-number request fact: a board-local number only
    /// resolves against a board, so it cannot mint the coordinate-less arm even from
    /// an agent slot. Dropping the `ById` operation match would let a by-number
    /// address take the global path, flipping this to `Some` — RED.
    #[test]
    fn a_by_number_address_cannot_mint_a_global_read() {
        assert!(authorize_global_by_id(&agent_slot("agent_1"), &by_number()).is_none());
        assert!(
            authorize_global_by_id(&conversation_frame("agent_1", "conv_1"), &by_number())
                .is_none()
        );
    }

    // ---- P0: lookup and authorization must name the SAME task, for EVERY actor ----

    fn is_identity_violation(failure: &PersistenceFailure) -> bool {
        std::error::Error::source(failure)
            .and_then(|source| source.downcast_ref::<AddressIdentityViolation>())
            .is_some()
    }

    fn is_address_provenance_violation(failure: &PersistenceFailure) -> bool {
        std::error::Error::source(failure)
            .and_then(|source| source.downcast_ref::<AddressProvenanceViolation>())
            .is_some()
    }

    /// HUMAN by-id: a read for `task_1` answered with `task_2` is refused as a safe-500
    /// even though the participation authority would GRANT — and refused BEFORE it is
    /// consulted (`calls == 0`), so a human who happens to participate in the other
    /// task's conversation is never handed it. The human arm has no witness to bind, so
    /// only the request-level identity gate can catch this: hoisting the gate above the
    /// actor dispatch is what makes it apply here. Deleting the `TaskReadAddress::names`
    /// id bind (or moving the gate back under the agent arm) reads `Some` — RED.
    #[tokio::test]
    async fn a_human_by_id_read_cannot_be_answered_with_a_different_task() {
        let other = task_on_full("task_2", 2, "ws:workspace_1", Some("workspace_1"));
        let (failure, calls) = refused_before_authorities(by_id_of("task_1"), human(), other).await;
        assert!(is_identity_violation(&failure));
        assert_eq!(calls, 0);
    }

    /// HUMAN by-number, NUMBER half: a read for number 99 on `workspace_1` answered with
    /// that board's task number 1 is a safe-500 before any authority is consulted.
    #[tokio::test]
    async fn a_human_by_number_read_binds_the_board_local_number() {
        let (failure, calls) =
            refused_before_authorities(by_number_on("workspace_1", 99), human(), workspace_task())
                .await;
        assert!(is_identity_violation(&failure));
        assert_eq!(calls, 0);
    }

    /// HUMAN by-number, BOARD half: the SAME number (1) on a DIFFERENT board is a
    /// different task, so a read for `(workspace_2, 1)` answered with the `workspace_1`
    /// task is a safe-500. A board-local number without its board is not a coordinate —
    /// dropping the `task.board() == board` half of `names` reads `Some` — RED. The
    /// request's board must come from resolving the request (the conversation's
    /// placement), never copied off the returned task.
    #[tokio::test]
    async fn a_human_by_number_read_binds_the_board_not_just_the_number() {
        let (failure, calls) =
            refused_before_authorities(by_number_on("workspace_2", 1), human(), workspace_task())
                .await;
        assert!(is_identity_violation(&failure));
        assert_eq!(calls, 0);
    }

    /// Positive controls for both human coordinates: the same paths with the coordinate
    /// the fetched task actually carries pass, so the falsifiers above are not passing
    /// for some unrelated reason.
    #[tokio::test]
    async fn a_human_read_for_the_real_coordinate_passes_on_both_addresses() {
        assert!(visible_at(by_id(), human(), workspace_task(), false, true, false).await);
        assert!(visible_at(by_number(), human(), workspace_task(), false, true, false).await);
    }

    /// AGENT by-id, end to end: a read for `task_1` answered with `task_2` whose
    /// participation AND capability both pass is refused as a safe-500 — so forging
    /// `ById(other_id)` buys nothing, because the read only ever authorizes the task that
    /// id names, which a caller reaches only by legitimately holding it.
    #[tokio::test]
    async fn a_global_read_cannot_be_answered_with_a_different_task_than_its_id() {
        let other = task_on_full("task_2", 2, "ws:workspace_1", Some("workspace_1"));
        let (failure, calls) =
            refused_before_authorities(by_id_of("task_1"), global_by_id("agent_1"), other).await;
        assert!(is_identity_violation(&failure));
        assert_eq!(calls, 0);
    }

    /// AGENT board-scoped by-number: the identity gate binds the board-local number on
    /// this arm too — a read for number 99 answered with the same board's task number 1
    /// is a safe-500 with participation and capability both passing.
    #[tokio::test]
    async fn a_board_scoped_by_number_read_binds_the_number_not_just_the_board() {
        let actor = board_scoped("agent_1", workspace_board("workspace_1"));
        let (failure, calls) =
            refused_before_authorities(by_number_on("workspace_1", 99), actor, workspace_task())
                .await;
        assert!(is_identity_violation(&failure));
        assert_eq!(calls, 0);
    }

    /// Positive control for the by-number board-scoped path: the number the fetched task
    /// actually carries passes all gates. This is the path a by-number caller takes for
    /// real — never through a temporarily-forged `ById`, which the provenance gate below
    /// refuses anyway.
    #[tokio::test]
    async fn a_board_scoped_by_number_read_for_the_real_number_passes() {
        let actor = board_scoped("agent_1", workspace_board("workspace_1"));
        assert!(visible_at(by_number(), actor, workspace_task(), true, false, true).await);
    }

    /// The coordinate-less witness is bound to the REQUEST it was minted for: a witness
    /// minted for `task_1` presented with a request for `task_2` (and the `task_2` row,
    /// so the identity gate itself is satisfied) is a safe-500 — the authorization and
    /// the lookup would otherwise have been decided by two different addresses. Deleting
    /// the `id == global.requested()` bind reads `Some` — RED.
    #[tokio::test]
    async fn a_global_witness_cannot_ride_a_request_for_another_id() {
        let other = task_on_full("task_2", 2, "ws:workspace_1", Some("workspace_1"));
        let actor = global_by_id_for("agent_1", &by_id_of("task_1"));
        let (failure, calls) = refused_before_authorities(by_id_of("task_2"), actor, other).await;
        assert!(is_address_provenance_violation(&failure));
        assert_eq!(calls, 0);
    }

    /// The reason the provenance bind exists: the witness's ONLY privilege is skipping
    /// the board-coordinate gate, so a by-number read must not be able to mint one from
    /// a throwaway `ById` and present it alongside its by-number request. The identity
    /// gate cannot catch this (the by-number address does name the returned task) — only
    /// the witness↔request bind can. Deleting it makes this read `Some`, i.e. a
    /// by-number read with NO coordinate gate — RED.
    #[tokio::test]
    async fn a_by_number_request_cannot_ride_a_coordinate_less_witness() {
        let actor = global_by_id_for("agent_1", &by_id_of("task_1"));
        let (failure, calls) =
            refused_before_authorities(by_number_on("workspace_1", 1), actor, workspace_task())
                .await;
        assert!(is_address_provenance_violation(&failure));
        assert_eq!(calls, 0);
    }

    // ---- P0-1 / P0-3: board-scope minting binds the subject --------------------

    struct Resolver {
        answer: BoardResolution,
    }

    impl AgentBoardResolver for Resolver {
        async fn resolve_board(
            &mut self,
            _request: &BoardScopeRequest,
        ) -> Result<BoardResolution, PersistenceFailure> {
            Ok(self.answer.clone())
        }
    }

    async fn mint(
        request: BoardScopeRequest,
        answer: BoardResolution,
    ) -> Result<BoardAuthorization, PersistenceFailure> {
        let mut resolver = Resolver { answer };
        authorize_agent_board(request, &mut resolver).await
    }

    /// Unwrap the authorized witness, panicking on either refusal — the success tests
    /// assert on the bound board, the refusal tests match the arm directly.
    fn authorized(auth: BoardAuthorization) -> AuthorizedAgentBoard {
        match auth {
            BoardAuthorization::Authorized(board) => board,
            BoardAuthorization::NoSuchWorkspace => {
                panic!("expected Authorized, got NoSuchWorkspace")
            }
            BoardAuthorization::Unauthorized => panic!("expected Authorized, got Unauthorized"),
        }
    }

    fn is_provenance_violation(failure: &PersistenceFailure) -> bool {
        std::error::Error::source(failure)
            .and_then(|source| source.downcast_ref::<ScopeProvenanceViolation>())
            .is_some()
    }

    /// Frame-ambient resolution: the authority resolves the frame's own board and
    /// echoes the request's subject; the minted witness carries the agent and board.
    #[tokio::test]
    async fn frame_ambient_resolution_mints_a_bound_board() {
        let frame = ConversationId::parse("conv_1").unwrap();
        let request = BoardScopeRequest {
            agent: agent_id("agent_1"),
            source: BoardScopeSource::FrameConversation(frame.clone()),
        };
        let witness = authorized(
            mint(
                request,
                BoardResolution::Resolved(ResolvedBoard {
                    agent: agent_id("agent_1"),
                    source: BoardScopeSource::FrameConversation(frame),
                    board: workspace_board("workspace_1"),
                }),
            )
            .await
            .unwrap(),
        );
        assert_eq!(witness.agent(), &agent_id("agent_1"));
        assert_eq!(witness.board(), &workspace_board("workspace_1"));
    }

    /// Explicitly-selected member workspace (a frame's other workspace or a slot's
    /// selector — same resolver path) mints a witness.
    #[tokio::test]
    async fn selected_member_workspace_mints_a_bound_board() {
        let request = BoardScopeRequest {
            agent: agent_id("agent_1"),
            source: BoardScopeSource::SelectedWorkspace(WorkspaceId::parse("workspace_2").unwrap()),
        };
        let witness = authorized(
            mint(
                request,
                BoardResolution::Resolved(ResolvedBoard {
                    agent: agent_id("agent_1"),
                    source: BoardScopeSource::SelectedWorkspace(
                        WorkspaceId::parse("workspace_2").unwrap(),
                    ),
                    board: workspace_board("workspace_2"),
                }),
            )
            .await
            .unwrap(),
        );
        assert_eq!(witness.board(), &workspace_board("workspace_2"));
    }

    /// An unauthorized selector (an existing workspace the agent is not a member of)
    /// resolves to [`BoardAuthorization::Unauthorized`] — a `403` denial, no witness.
    #[tokio::test]
    async fn an_unauthorized_selector_mints_no_witness() {
        let request = BoardScopeRequest {
            agent: agent_id("agent_1"),
            source: BoardScopeSource::SelectedWorkspace(WorkspaceId::parse("workspace_9").unwrap()),
        };
        assert!(matches!(
            mint(request, BoardResolution::Unauthorized).await.unwrap(),
            BoardAuthorization::Unauthorized
        ));
    }

    /// A selector naming no such workspace resolves to
    /// [`BoardAuthorization::NoSuchWorkspace`] — a `404`, carried through verbatim so the
    /// surface can distinguish it from the `403` non-member refusal.
    #[tokio::test]
    async fn an_unknown_workspace_selector_is_carried_through_as_not_found() {
        let request = BoardScopeRequest {
            agent: agent_id("agent_1"),
            source: BoardScopeSource::SelectedWorkspace(WorkspaceId::parse("workspace_9").unwrap()),
        };
        assert!(matches!(
            mint(request, BoardResolution::NoSuchWorkspace)
                .await
                .unwrap(),
            BoardAuthorization::NoSuchWorkspace
        ));
    }

    /// An authority that answers for a DIFFERENT agent than requested mints no
    /// witness — it is a subject-confusion safe-500, not a silent A→B splice.
    #[tokio::test]
    async fn a_board_answered_for_another_agent_is_a_safe_500() {
        let frame = ConversationId::parse("conv_1").unwrap();
        let request = BoardScopeRequest {
            agent: agent_id("agent_1"),
            source: BoardScopeSource::FrameConversation(frame.clone()),
        };
        let failure = mint(
            request,
            BoardResolution::Resolved(ResolvedBoard {
                agent: agent_id("agent_2"), // answered for a different principal
                source: BoardScopeSource::FrameConversation(frame),
                board: workspace_board("workspace_1"),
            }),
        )
        .await
        .err()
        .unwrap();
        assert!(is_provenance_violation(&failure));
    }

    /// An authority that answers about a DIFFERENT source than requested mints no
    /// witness — a fact resolved for conversation B cannot be spliced onto a request
    /// that named conversation A.
    #[tokio::test]
    async fn a_board_answered_for_another_source_is_a_safe_500() {
        let request = BoardScopeRequest {
            agent: agent_id("agent_1"),
            source: BoardScopeSource::FrameConversation(ConversationId::parse("conv_a").unwrap()),
        };
        let failure = mint(
            request,
            BoardResolution::Resolved(ResolvedBoard {
                agent: agent_id("agent_1"),
                source: BoardScopeSource::FrameConversation(
                    ConversationId::parse("conv_b").unwrap(),
                ),
                board: workspace_board("workspace_1"),
            }),
        )
        .await
        .err()
        .unwrap();
        assert!(is_provenance_violation(&failure));
    }

    // ---- Union list arm: the enumeration binds each board to the requested agent ----

    /// A fake board enumerator returning a fixed list — the SQLite adapter's core-side
    /// stand-in, so this turns on the minter's subject-bind, not any query.
    struct Enumerator(Vec<ReadableBoard>);

    impl AgentReadableBoards for Enumerator {
        async fn readable_boards(
            &mut self,
            _agent: &AgentId,
        ) -> Result<Vec<ReadableBoard>, PersistenceFailure> {
            Ok(self.0.clone())
        }
    }

    fn pair_board(key: &str) -> BoardKey {
        BoardKey::Pair(crate::conversation_messaging::messaging::task::PairKey::parse(key).unwrap())
    }

    /// The happy path: every enumerated board is echoed for the requested agent, so the
    /// minter passes each through as an `AuthorizedAgentBoard` bound to that agent — the
    /// union of workspace and direct-pair boards a later `authorize_task_list_boards`
    /// folds the `read_tasks` capability into.
    #[tokio::test]
    async fn enumerated_boards_bind_to_the_requested_agent() {
        let agent = agent_id("agent_1");
        let mut enumerator = Enumerator(vec![
            ReadableBoard {
                agent: agent.clone(),
                board: workspace_board("workspace_1"),
            },
            ReadableBoard {
                agent: agent.clone(),
                board: pair_board("pair_1"),
            },
        ]);
        let authorized = authorize_readable_agent_boards(&agent, &mut enumerator)
            .await
            .expect("a same-agent enumeration mints cleanly");
        let boards: Vec<&BoardKey> = authorized.iter().map(|board| board.board()).collect();
        assert_eq!(
            boards,
            [&workspace_board("workspace_1"), &pair_board("pair_1")]
        );
        assert!(authorized.iter().all(|board| board.agent() == &agent));
    }

    /// An entry enumerated for a DIFFERENT agent than requested is a wired-wrong adapter,
    /// not a caller denial: the same subject-confusion safe-500 the single-board minter
    /// raises, so no witness escapes bound to the wrong subject.
    #[tokio::test]
    async fn an_enumerated_entry_for_another_agent_is_a_safe_500() {
        let agent = agent_id("agent_1");
        let mut enumerator = Enumerator(vec![
            ReadableBoard {
                agent: agent.clone(),
                board: workspace_board("workspace_1"),
            },
            ReadableBoard {
                agent: agent_id("agent_2"),
                board: pair_board("pair_1"),
            },
        ]);
        let failure = authorize_readable_agent_boards(&agent, &mut enumerator)
            .await
            .err()
            .expect("a foreign-agent entry is a safe-500");
        assert!(is_provenance_violation(&failure));
    }

    /// An empty enumeration is an agent with no readable board — an empty authorized set,
    /// never an error, which lists as nothing downstream.
    #[tokio::test]
    async fn an_empty_enumeration_is_an_empty_authorized_set() {
        let agent = agent_id("agent_1");
        let mut enumerator = Enumerator(Vec::new());
        let authorized = authorize_readable_agent_boards(&agent, &mut enumerator)
            .await
            .unwrap();
        assert!(authorized.is_empty());
    }

    // ---- P0-4: the success fact keeps its subject ------------------------------

    /// A task authorized for one principal binds to THAT principal: `for_viewer`
    /// yields the task only to it, and `None` to any other viewer — an A-authorized
    /// task cannot drive B's viewer-resolved projection.
    #[tokio::test]
    async fn a_visible_task_binds_to_its_authorized_viewer() {
        let user_a = UserId::parse("user_1").unwrap();
        let mut participation = Participation {
            agent: false,
            user: true,
        };
        let mut capability = Capability { grant: false };
        let visible = read_visible_task_on(
            &request(by_id(), TaskReadActor::User(user_a.clone())),
            workspace_task(),
            &mut TestAuthority {
                participation: &mut participation,
                capability: &mut capability,
            },
        )
        .await
        .unwrap()
        .expect("the participating human is authorized");

        assert_eq!(visible.viewer(), &ReadViewer::User(user_a.clone()));
        assert!(visible.for_viewer(&ReadViewer::User(user_a)).is_some());
        assert!(
            visible
                .for_viewer(&ReadViewer::User(UserId::parse("user_2").unwrap()))
                .is_none()
        );
        assert!(
            visible
                .for_viewer(&ReadViewer::Agent(agent_id("agent_1")))
                .is_none()
        );
    }

    /// The agent success fact carries the scope's agent as its viewer.
    #[tokio::test]
    async fn an_agent_visible_task_carries_the_scope_agent_as_viewer() {
        let actor = board_scoped("agent_1", workspace_board("workspace_1"));
        let mut participation = Participation {
            agent: true,
            user: false,
        };
        let mut capability = Capability { grant: true };
        let visible = read_visible_task_on(
            &request(by_id(), actor),
            workspace_task(),
            &mut TestAuthority {
                participation: &mut participation,
                capability: &mut capability,
            },
        )
        .await
        .unwrap()
        .expect("the participating, capable agent is authorized");

        assert_eq!(visible.viewer(), &ReadViewer::Agent(agent_id("agent_1")));
        assert!(
            visible
                .for_viewer(&ReadViewer::Agent(agent_id("agent_2")))
                .is_none()
        );
    }

    // ---- Board list -------------------------------------------------------------

    /// A candidate fixture with an explicit id, board-local number, board, conversation,
    /// and creation time — the axes the list's ordering and per-row gates turn on.
    fn list_task(
        id: &str,
        task_number: i64,
        board_key: &str,
        workspace_id: Option<&str>,
        conversation: &str,
        created_at: i64,
    ) -> TaskAggregate {
        TaskAggregate::decode(TaskRow {
            id: id.into(),
            task_number,
            conversation_id: conversation.into(),
            thread_root_id: None,
            board_key: board_key.into(),
            workspace_id: workspace_id.map(Into::into),
            title: "a title".into(),
            content: String::new(),
            status: "todo".into(),
            assignee_agent_id: None,
            assignee_user_id: None,
            creator_kind: "system".into(),
            creator_agent_id: None,
            creator_user_id: None,
            source_message_id: None,
            created_event_message_id: None,
            last_editor_kind: None,
            last_editor_id: None,
            last_edited_at: None,
            created_at,
            closed_at: None,
        })
        .expect("the fixture row is a valid committed fact")
    }

    /// A workspace-board candidate in `conv_1`.
    fn ws_task(id: &str, number: i64, workspace: &str, created_at: i64) -> TaskAggregate {
        list_task(
            id,
            number,
            &format!("ws:{workspace}"),
            Some(workspace),
            "conv_1",
            created_at,
        )
    }

    /// Wrap decoded aggregates into list candidates with empty display sidecars — the
    /// sidecar is inert to visibility, ordering, and trimming (the axes these tests turn
    /// on), so an empty one exercises the authority path without a joined fetch.
    fn list_candidates(tasks: Vec<TaskAggregate>) -> Vec<TaskListCandidate> {
        tasks
            .into_iter()
            .map(|task| {
                TaskListCandidate::new(
                    task,
                    TaskDisplaySidecars {
                        channel_name: None,
                        assignee_agent_name: None,
                        assignee_user_name: None,
                        creator_agent_name: None,
                        creator_user_name: None,
                    },
                )
            })
            .collect()
    }

    /// Mint the authorized board set for `agent` over `boards`, folding `grant` as the
    /// `read_tasks` verdict. The board witnesses are minted through the module-private
    /// tuple constructor (a child module sees it), exactly as `board_scoped` does.
    async fn agent_scope(agent: &str, boards: Vec<BoardKey>, grant: bool) -> AuthorizedTaskBoards {
        let witnesses = boards
            .into_iter()
            .map(|board| AuthorizedAgentBoard(agent_id(agent), board))
            .collect();
        let mut capability = Capability { grant };
        authorize_task_list_boards(agent_id(agent), witnesses, &mut capability)
            .await
            .expect("a same-agent board set with a granting capability mints cleanly")
    }

    /// The surviving rows' ids, in order — read through each row's own projection bundle,
    /// so a change that dropped the aggregate from the row would fail to compile here.
    fn ordered_ids(visible: &[VisibleTaskListRow]) -> Vec<String> {
        visible
            .iter()
            .map(|row| row.summary().task.id().as_str().to_string())
            .collect()
    }

    /// Participation keyed by conversation id, granting for a fixed set — so a test can
    /// make some rows participate and others not, independent of their order.
    struct ParticipatingConversations(Vec<&'static str>);

    impl TaskParticipationReads for ParticipatingConversations {
        async fn is_agent_participant(
            &mut self,
            conversation: &ConversationId,
            _agent: &AgentId,
        ) -> Result<bool, PersistenceFailure> {
            Ok(self.0.iter().any(|id| *id == conversation.as_str()))
        }

        async fn is_user_participant(
            &mut self,
            conversation: &ConversationId,
            _user: &UserId,
        ) -> Result<bool, PersistenceFailure> {
            Ok(self.0.iter().any(|id| *id == conversation.as_str()))
        }

        async fn agent_participating_conversations(
            &mut self,
            agent: &AgentId,
            conversations: &[ConversationId],
        ) -> Result<HashSet<ConversationId>, PersistenceFailure> {
            let mut set = HashSet::new();
            for conversation in conversations {
                if self.is_agent_participant(conversation, agent).await? {
                    set.insert(conversation.clone());
                }
            }
            Ok(set)
        }
    }

    #[tokio::test]
    async fn a_single_board_agent_orders_by_number_desc() {
        let scope = agent_scope("agent_1", vec![workspace_board("workspace_1")], true).await;
        let candidates = list_candidates(vec![
            ws_task("task_2", 2, "workspace_1", 999),
            ws_task("task_1", 1, "workspace_1", 100),
            ws_task("task_3", 3, "workspace_1", 200),
        ]);
        let mut participation = Participation {
            agent: true,
            user: false,
        };
        let visible = list_visible_tasks(&scope, candidates, &mut participation, None)
            .await
            .unwrap();
        // Single board => number DESC, NOT created_at DESC (which would put task_2 first).
        assert_eq!(ordered_ids(&visible), ["task_3", "task_2", "task_1"]);
    }

    #[tokio::test]
    async fn a_multi_board_agent_orders_by_created_at_desc_then_id_desc() {
        let scope = agent_scope(
            "agent_1",
            vec![
                workspace_board("workspace_1"),
                workspace_board("workspace_2"),
            ],
            true,
        )
        .await;
        let candidates = list_candidates(vec![
            ws_task("task_a", 9, "workspace_1", 100),
            ws_task("task_b", 1, "workspace_2", 300),
            // Equal created_at with task_b: the id DESC tiebreak must order task_c first.
            ws_task("task_c", 2, "workspace_1", 300),
        ]);
        let mut participation = Participation {
            agent: true,
            user: false,
        };
        let visible = list_visible_tasks(&scope, candidates, &mut participation, None)
            .await
            .unwrap();
        assert_eq!(ordered_ids(&visible), ["task_c", "task_b", "task_a"]);
    }

    #[tokio::test]
    async fn an_agent_row_off_the_authorized_boards_is_dropped_even_when_participating() {
        // A row on workspace_2 leaks past the fetch's WHERE; the per-row coordinate
        // re-bind against the row's OWN board keeps it out of a workspace_1-only scope,
        // though the agent participates in its conversation.
        let scope = agent_scope("agent_1", vec![workspace_board("workspace_1")], true).await;
        let candidates = list_candidates(vec![
            ws_task("on_board", 1, "workspace_1", 100),
            ws_task("off_board", 2, "workspace_2", 200),
        ]);
        let mut participation = Participation {
            agent: true,
            user: false,
        };
        let visible = list_visible_tasks(&scope, candidates, &mut participation, None)
            .await
            .unwrap();
        assert_eq!(ordered_ids(&visible), ["on_board"]);
    }

    #[tokio::test]
    async fn an_agent_non_participating_row_is_dropped() {
        let scope = agent_scope("agent_1", vec![workspace_board("workspace_1")], true).await;
        let mut participation = Participation {
            agent: false,
            user: false,
        };
        let visible = list_visible_tasks(
            &scope,
            list_candidates(vec![ws_task("task_1", 1, "workspace_1", 100)]),
            &mut participation,
            None,
        )
        .await
        .unwrap();
        assert!(visible.is_empty());
    }

    #[tokio::test]
    async fn a_capability_less_workspace_board_never_enters_the_scope() {
        // grant = false => the workspace board is folded OUT at mint, so the scope is
        // empty and the participating, on-board row still lists as nothing.
        let scope = agent_scope("agent_1", vec![workspace_board("workspace_1")], false).await;
        let mut participation = Participation {
            agent: true,
            user: false,
        };
        let visible = list_visible_tasks(
            &scope,
            list_candidates(vec![ws_task("task_1", 1, "workspace_1", 100)]),
            &mut participation,
            None,
        )
        .await
        .unwrap();
        assert!(visible.is_empty());
    }

    #[tokio::test]
    async fn a_direct_pair_board_enters_the_scope_without_a_capability_axis() {
        // A pair board has no capability axis, so it is admitted without ever consulting
        // the capability authority — NeverCapability panics if that promise is broken.
        let witnesses = vec![AuthorizedAgentBoard(
            agent_id("agent_1"),
            BoardKey::Pair(
                crate::conversation_messaging::messaging::task::PairKey::parse("pair_1").unwrap(),
            ),
        )];
        let mut capability = NeverCapability;
        let scope = authorize_task_list_boards(agent_id("agent_1"), witnesses, &mut capability)
            .await
            .expect("a direct-pair board is admitted on participation alone");
        let mut participation = Participation {
            agent: true,
            user: false,
        };
        let visible = list_visible_tasks(
            &scope,
            list_candidates(vec![list_task(
                "pair_task",
                1,
                "pair:pair_1",
                None,
                "conv_1",
                100,
            )]),
            &mut participation,
            None,
        )
        .await
        .unwrap();
        assert_eq!(ordered_ids(&visible), ["pair_task"]);
    }

    #[tokio::test]
    async fn a_board_authorized_for_another_agent_is_a_safe_500() {
        // A board witness minted for agent_2 cannot be folded into agent_1's list scope:
        // a subject-confused set is a wired-wrong call site, not a caller denial.
        let witnesses = vec![AuthorizedAgentBoard(
            agent_id("agent_2"),
            workspace_board("workspace_1"),
        )];
        let mut capability = Capability { grant: true };
        assert!(
            authorize_task_list_boards(agent_id("agent_1"), witnesses, &mut capability)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn limit_trims_to_the_top_visible_rows() {
        let scope = agent_scope("agent_1", vec![workspace_board("workspace_1")], true).await;
        let candidates = vec![
            ws_task("task_1", 1, "workspace_1", 100),
            ws_task("task_2", 2, "workspace_1", 200),
            ws_task("task_3", 3, "workspace_1", 300),
        ];
        let mut participation = Participation {
            agent: true,
            user: false,
        };
        let visible = list_visible_tasks(
            &scope,
            list_candidates(candidates),
            &mut participation,
            Some(2),
        )
        .await
        .unwrap();
        assert_eq!(ordered_ids(&visible), ["task_3", "task_2"]);
    }

    #[tokio::test]
    async fn limit_counts_visible_rows_not_fetched_ones() {
        // task_2 does not participate; it must NOT consume a limit slot, so a limit of 2
        // still reaches the two participating rows below it in order.
        let scope = agent_scope("agent_1", vec![workspace_board("workspace_1")], true).await;
        let candidates = vec![
            list_task(
                "task_3",
                3,
                "ws:workspace_1",
                Some("workspace_1"),
                "seen",
                300,
            ),
            list_task(
                "task_2",
                2,
                "ws:workspace_1",
                Some("workspace_1"),
                "hidden",
                200,
            ),
            list_task(
                "task_1",
                1,
                "ws:workspace_1",
                Some("workspace_1"),
                "seen",
                100,
            ),
        ];
        let mut participation = ParticipatingConversations(vec!["seen"]);
        let visible = list_visible_tasks(
            &scope,
            list_candidates(candidates),
            &mut participation,
            Some(2),
        )
        .await
        .unwrap();
        assert_eq!(ordered_ids(&visible), ["task_3", "task_1"]);
    }

    /// A capability authority that records every consultation — proves the minter dedups
    /// before folding, so a board authorized twice asks the external read only once.
    struct CountingCapability {
        calls: usize,
        grant: bool,
    }

    impl TaskReadCapability for CountingCapability {
        async fn may_read_tasks(
            &mut self,
            _agent: &AgentId,
            _workspace: &WorkspaceId,
        ) -> Result<bool, PersistenceFailure> {
            self.calls += 1;
            Ok(self.grant)
        }

        async fn readable_task_workspaces(
            &mut self,
            _agent: &AgentId,
            workspaces: &[WorkspaceId],
        ) -> Result<HashSet<WorkspaceId>, PersistenceFailure> {
            // ONE round-trip for the whole distinct workspace-board set.
            self.calls += 1;
            Ok(if self.grant {
                workspaces.iter().cloned().collect()
            } else {
                HashSet::new()
            })
        }
    }

    #[tokio::test]
    async fn duplicate_witnesses_for_one_board_stay_single_board_number_order() {
        // Two legitimate, same-agent witnesses for the SAME board must NOT classify the
        // scope as multi-board: the set is one distinct board, so it orders by
        // task_number DESC (#3 -> #1), never by created_at DESC (which the timestamps
        // below would flip to #1 -> #3).
        let witnesses = vec![
            AuthorizedAgentBoard(agent_id("agent_1"), workspace_board("workspace_1")),
            AuthorizedAgentBoard(agent_id("agent_1"), workspace_board("workspace_1")),
        ];
        let mut capability = Capability { grant: true };
        let boards = authorize_task_list_boards(agent_id("agent_1"), witnesses, &mut capability)
            .await
            .unwrap();
        assert!(!boards.is_multi_board());

        let scope = boards;
        let candidates = vec![
            ws_task("task_3", 3, "workspace_1", 100),
            ws_task("task_1", 1, "workspace_1", 999),
        ];
        let mut participation = Participation {
            agent: true,
            user: false,
        };
        let visible = list_visible_tasks(
            &scope,
            list_candidates(candidates),
            &mut participation,
            None,
        )
        .await
        .unwrap();
        assert_eq!(ordered_ids(&visible), ["task_3", "task_1"]);
    }

    #[tokio::test]
    async fn the_capability_read_is_one_round_trip_regardless_of_board_count() {
        // FAN-OUT FALSIFIER: the `read_tasks` fold is a single set read, so growing the
        // DISTINCT board count leaves the authority round-trip count at ONE. A regression
        // to a per-board `may_read_tasks` loop would make `calls` track the board count.
        let witnesses: Vec<_> = (0..12)
            .map(|n| {
                AuthorizedAgentBoard(
                    agent_id("agent_1"),
                    workspace_board(&format!("workspace_{n}")),
                )
            })
            .collect();
        let mut capability = CountingCapability {
            calls: 0,
            grant: true,
        };
        let boards = authorize_task_list_boards(agent_id("agent_1"), witnesses, &mut capability)
            .await
            .unwrap();
        assert_eq!(
            capability.calls, 1,
            "twelve distinct boards, one capability read"
        );
        assert_eq!(boards.fetch_coordinate().len(), 12);
    }

    #[tokio::test]
    async fn the_participation_read_is_one_round_trip_regardless_of_candidate_count() {
        // FAN-OUT FALSIFIER: participation is gated as a single set read over the on-board
        // candidates' conversations, so growing the candidate count leaves the round-trip
        // count at ONE. A regression to the per-row `is_agent_participant` loop would make
        // `calls` track the candidate count.
        let scope = agent_scope("agent_1", vec![workspace_board("workspace_1")], true).await;
        let candidates = list_candidates(
            (1..=12)
                .map(|n| {
                    list_task(
                        &format!("task_{n}"),
                        n,
                        "ws:workspace_1",
                        Some("workspace_1"),
                        &format!("conv_{n}"),
                        100 + n,
                    )
                })
                .collect(),
        );
        let mut participation = CountingParticipation { calls: 0 };
        let visible = list_visible_tasks(&scope, candidates, &mut participation, None)
            .await
            .unwrap();
        assert_eq!(
            participation.calls, 1,
            "twelve candidates across twelve conversations, one participation read"
        );
        // The set read grants all, so every on-board candidate is visible.
        assert_eq!(visible.len(), 12);
    }
}
