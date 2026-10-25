//! The agent task-list application read: from an authenticated
//! [`RuntimePrincipal`] plus an optional `--workspace-id` selector, resolve the
//! opaque board scope the list is confined to, then hand the server the witness it
//! projects the candidate fetch's coordinate from.
//!
//! This module owns ONE domain decision the wire must not: which of the three board
//! arms a list runs under, and when a refused arm is a hard denial versus an empty
//! page. The arms are the parity of the TS route's `resolveAgentQueryWorkspaceId` +
//! its three list branches:
//!
//!  - **explicit `workspace_id` selector** → a SINGLE validated workspace board (a
//!    frame or a slot principal may pass one);
//!  - else a **conversation-frame** principal → the frame's OWN board, derived from
//!    its placement (a direct-pair board or its workspace channel's board);
//!  - else an **agent-slot** principal → the UNION of every board the agent may read.
//!
//! Each arm mints the same opaque [`AuthorizedTaskBoards`] the fetch coordinate is
//! projected from ([`resolve_task_list_boards`] returns the witness, never a bare
//! board list), so a list entry point can never be handed an unbacked `Vec<BoardKey>`.
//!
//! ## Refusal semantics (context over control)
//!
//! The UNION arm FILTERS by capability: a workspace the agent belongs to but holds no
//! `read_tasks` on is silently dropped from the union, and an agent with no readable
//! board lists as nothing — an empty `200`, exactly as the TS union does. A SINGLE
//! named board is different: the agent asked one board a direct question, so a board
//! it may not read is a refusal, not a misleading empty page. This module distinguishes
//! the three TS single-board refusals — matching `resolveAgentQueryWorkspaceId`'s route
//! branches — so the surface can return the reason the server already knows (`context
//! over control`; the enumeration this exposes is an accepted tradeoff):
//!
//!  - a selector naming NO SUCH workspace → [`TaskListAccess::UnknownWorkspace`] (`404`);
//!  - a workspace that exists but the agent is not a member of →
//!    [`TaskListForbidden::NotAWorkspaceMember`] (`403`);
//!  - a member workspace the agent holds no `read_tasks` on →
//!    [`TaskListForbidden::ReadTasksDenied`] (`403`);
//!  - a frame that derives no board → [`TaskListForbidden::NoReadableBoard`] (`403`).
//!
//! The `404`/`403` split is decided by the board-scope resolver — the one authority that
//! reads the workspace row (see `BoardResolution`) — never by the wire remembering to
//! merge codes.

use std::future::Future;

use crate::foundation::identity::{AgentId, ConversationId, WorkspaceId};
use crate::foundation::persistence::PersistenceFailure;
use crate::host_runtime::runtime_credential::RuntimePrincipal;

use super::read::{
    AgentBoardResolver, AgentReadableBoards, AuthorizedTaskBoards, BoardAuthorization,
    BoardScopeRequest, BoardScopeSource, TaskListCandidate, TaskReadCapability,
    authorize_agent_board, authorize_readable_agent_boards, authorize_task_list_boards,
};
use super::{BoardKey, TaskNumber, TaskStatus};

/// The COORDINATE filters a board-list fetch narrows by — the parity of the TS
/// `ListTasksFilters` MINUS the participation predicate, ordering, and limit the core
/// owns. `boards` is the agent's already-authorized board-key set (the server projects
/// it from the opaque [`AuthorizedTaskBoards`] witness via
/// [`AuthorizedTaskBoards::fetch_coordinate`]); an empty set reads as zero rows. Every
/// other field is a `WHERE` conjunct; an absent/empty field adds no clause.
///
/// It is a domain coordinate — validated identities and closed enums, no driver types —
/// so it lives in core beside the authority that produces its `boards`; the SQLite
/// [`TaskCandidateFetch`] adapter consumes it verbatim.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskCandidateQuery {
    /// The board coordinate: the agent's authorized board keys. Empty = zero rows. The
    /// core re-binds each row's own board against the authorized set, so this is a
    /// coordinate narrowing, not the authorization boundary.
    pub boards: Vec<BoardKey>,
    /// Narrow to a single conversation (`t.conversation_id = ?`).
    pub conversation: Option<ConversationId>,
    /// Narrow to a set of statuses (`t.status IN (...)`); empty = every status.
    pub statuses: Vec<TaskStatus>,
    /// Narrow to a set of board-local numbers (`t.task_number IN (...)`); empty = every.
    pub task_numbers: Vec<TaskNumber>,
    /// Narrow to one assignee agent (`t.assignee_agent_id = ?`).
    pub assignee_agent: Option<AgentId>,
}

/// The consumer port that reads the DECODED candidate set a
/// [`list_visible_tasks`](super::read::list_visible_tasks) call trims — every `tasks`
/// row matching the query's coordinate filters, strict-decoded and paired with its
/// display sidecars. It carries NO participation predicate, ordering, or limit (all the
/// core's), so the returned set is a superset of what the viewer will see. The one
/// implementation is the SQLite task store; the HTTP boundary depends only on this port.
pub trait TaskCandidateFetch {
    fn fetch_list_candidates(
        &self,
        query: &TaskCandidateQuery,
    ) -> impl Future<Output = Result<Vec<TaskListCandidate>, PersistenceFailure>> + Send;
}

/// The outcome of resolving an agent list's board scope: either the opaque witness the
/// fetch coordinate is projected from, or a refusal for a single named board the agent
/// may not read (see the module docs — the union arm never refuses, it lists as
/// nothing). The refusals carry the REASON the server knows so the surface can answer
/// `404` for an unknown selector and `403` for an existing-but-unreachable one (context
/// over control).
pub enum TaskListAccess {
    /// The authorized board scope — possibly empty (the union arm lists as nothing).
    Authorized(AuthorizedTaskBoards),
    /// The selected workspace does not exist — the server answers `404`.
    UnknownWorkspace(WorkspaceId),
    /// A single named board the agent may not read — the server answers `403`.
    Forbidden(TaskListForbidden),
}

/// Why a single named board is refused with a `403`. Each carries the reason the TS
/// route distinguishes so the wire can render the matching code/message.
pub enum TaskListForbidden {
    /// The workspace exists but the agent is not a member of it
    /// (`workspace_membership_required`).
    NotAWorkspaceMember(WorkspaceId),
    /// The agent is a member but holds no `read_tasks` capability (`capability_denied`).
    ReadTasksDenied(WorkspaceId),
    /// A frame that derives no readable board — no workspace identity to name.
    NoReadableBoard,
}

/// Resolve the board scope an agent task list is confined to, minting the opaque
/// [`AuthorizedTaskBoards`] the caller projects the fetch coordinate from.
///
/// `selector` is an explicit board source (`--workspace-id/--workspace`, or a
/// `--target` conversation the caller already resolved); `principal` is the
/// authenticated runtime. The agent is ALWAYS taken from the principal, never from a
/// URL or caller-supplied field, so a list cannot be widened to another agent's boards.
/// See the module docs for the three arms and the union-vs-single refusal semantics.
pub async fn resolve_task_list_boards<P>(
    principal: &RuntimePrincipal,
    selector: Option<&BoardScopeSource>,
    ports: &mut P,
) -> Result<TaskListAccess, PersistenceFailure>
where
    P: AgentBoardResolver + AgentReadableBoards + TaskReadCapability + Send,
{
    let agent = principal.agent().clone();

    // An explicit selector is a single validated workspace board for either principal
    // kind; otherwise a conversation frame reads its own board and an agent slot the
    // union — the parity of `resolveAgentQueryWorkspaceId` + the route's three arms.
    let single_source = match selector {
        Some(source) => Some(source.clone()),
        None => principal
            .source()
            .map(|frame| BoardScopeSource::FrameConversation(frame.clone())),
    };

    match single_source {
        Some(source) => {
            // A selector can name a non-existent workspace (→ 404); a frame's source is
            // a real conversation, so its refusals are always 403. Keep the selected id
            // to name it in the refusal the surface renders.
            let selected_workspace = match &source {
                BoardScopeSource::SelectedWorkspace(workspace) => Some(workspace.clone()),
                BoardScopeSource::FrameConversation(_) => None,
            };
            let request = BoardScopeRequest {
                agent: agent.clone(),
                source,
            };
            let witness = match authorize_agent_board(request, &mut *ports).await? {
                BoardAuthorization::Authorized(witness) => witness,
                BoardAuthorization::NoSuchWorkspace => {
                    // Only a selector reaches this; a frame arm answering it would be a
                    // wired-wrong adapter, refused as a plain 403 rather than a false 404.
                    return Ok(match selected_workspace {
                        Some(workspace) => TaskListAccess::UnknownWorkspace(workspace),
                        None => TaskListAccess::Forbidden(TaskListForbidden::NoReadableBoard),
                    });
                }
                BoardAuthorization::Unauthorized => {
                    return Ok(TaskListAccess::Forbidden(match selected_workspace {
                        Some(workspace) => TaskListForbidden::NotAWorkspaceMember(workspace),
                        None => TaskListForbidden::NoReadableBoard,
                    }));
                }
            };
            let boards = authorize_task_list_boards(agent, vec![witness], &mut *ports).await?;
            // The one named board was dropped by the `read_tasks` fold: a member with no
            // capability, a direct 403 rather than the empty page the union arm gives.
            if boards.fetch_coordinate().is_empty() {
                return Ok(TaskListAccess::Forbidden(match selected_workspace {
                    Some(workspace) => TaskListForbidden::ReadTasksDenied(workspace),
                    None => TaskListForbidden::NoReadableBoard,
                }));
            }
            Ok(TaskListAccess::Authorized(boards))
        }
        None => {
            let witnesses = authorize_readable_agent_boards(&agent, &mut *ports).await?;
            // The union filters by capability and may be empty — that lists as nothing.
            let boards = authorize_task_list_boards(agent, witnesses, &mut *ports).await?;
            Ok(TaskListAccess::Authorized(boards))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_messaging::messaging::task::{
        BoardResolution, PairKey, ReadableBoard, ResolvedBoard,
    };
    use crate::foundation::identity::DaemonInstanceId;
    use crate::host_runtime::runtime_credential::{RuntimeCustody, RuntimeGeneration};

    /// A configurable driver-free double for the three board-authorization ports. The
    /// resolver echoes the requested subject (so the minter binds it), returning the
    /// configured board when authorized; no board yields an unauthorized scope.
    /// The enumerator returns the configured union; the capability answers one flag for
    /// every workspace. It records the source the single-board arm asked for, so arm
    /// dispatch is observable without a database.
    struct FakePorts {
        resolved_board: Option<BoardKey>,
        readable: Vec<BoardKey>,
        read_tasks: bool,
        observed_source: Option<BoardScopeSource>,
    }

    impl FakePorts {
        fn new() -> Self {
            Self {
                resolved_board: None,
                readable: Vec::new(),
                read_tasks: true,
                observed_source: None,
            }
        }
    }

    impl AgentBoardResolver for FakePorts {
        async fn resolve_board(
            &mut self,
            request: &BoardScopeRequest,
        ) -> Result<BoardResolution, PersistenceFailure> {
            self.observed_source = Some(request.source.clone());
            Ok(match self.resolved_board.clone() {
                Some(board) => BoardResolution::Resolved(ResolvedBoard {
                    agent: request.agent.clone(),
                    source: request.source.clone(),
                    board,
                }),
                None => BoardResolution::Unauthorized,
            })
        }
    }

    impl AgentReadableBoards for FakePorts {
        async fn readable_boards(
            &mut self,
            agent: &AgentId,
        ) -> Result<Vec<ReadableBoard>, PersistenceFailure> {
            Ok(self
                .readable
                .iter()
                .cloned()
                .map(|board| ReadableBoard {
                    agent: agent.clone(),
                    board,
                })
                .collect())
        }
    }

    impl TaskReadCapability for FakePorts {
        async fn may_read_tasks(
            &mut self,
            _agent: &AgentId,
            _workspace: &WorkspaceId,
        ) -> Result<bool, PersistenceFailure> {
            Ok(self.read_tasks)
        }

        async fn readable_task_workspaces(
            &mut self,
            _agent: &AgentId,
            workspaces: &[WorkspaceId],
        ) -> Result<std::collections::HashSet<WorkspaceId>, PersistenceFailure> {
            Ok(if self.read_tasks {
                workspaces.iter().cloned().collect()
            } else {
                std::collections::HashSet::new()
            })
        }
    }

    fn custody() -> RuntimeCustody {
        RuntimeCustody::new(
            RuntimeGeneration::new(0).unwrap(),
            DaemonInstanceId::parse("daemon-1").unwrap(),
        )
    }

    fn agent_slot(agent: &str) -> RuntimePrincipal {
        RuntimePrincipal::AgentSlot {
            agent: AgentId::parse(agent).unwrap(),
            custody: custody(),
        }
    }

    fn conversation_frame(agent: &str, source: &str) -> RuntimePrincipal {
        RuntimePrincipal::ConversationSlot {
            agent: AgentId::parse(agent).unwrap(),
            source: ConversationId::parse(source).unwrap(),
            custody: custody(),
        }
    }

    fn ws_board(raw: &str) -> BoardKey {
        BoardKey::Workspace(WorkspaceId::parse(raw).unwrap())
    }

    fn pair_board(raw: &str) -> BoardKey {
        BoardKey::Pair(PairKey::parse(raw).unwrap())
    }

    fn authorized(access: TaskListAccess) -> Vec<BoardKey> {
        match access {
            TaskListAccess::Authorized(boards) => boards.fetch_coordinate(),
            TaskListAccess::UnknownWorkspace(_) => {
                panic!("expected an authorized scope, got UnknownWorkspace")
            }
            TaskListAccess::Forbidden(_) => {
                panic!("expected an authorized scope, got Forbidden")
            }
        }
    }

    #[tokio::test]
    async fn agent_slot_without_a_selector_unions_every_readable_board() {
        let mut ports = FakePorts::new();
        ports.readable = vec![ws_board("ws-1"), ws_board("ws-2")];
        let access = resolve_task_list_boards(&agent_slot("agent-1"), None, &mut ports)
            .await
            .unwrap();
        let mut boards = authorized(access);
        boards.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
        assert_eq!(boards, vec![ws_board("ws-1"), ws_board("ws-2")]);
        // The union arm never touched the single-board resolver.
        assert_eq!(ports.observed_source, None);
    }

    #[tokio::test]
    async fn an_empty_union_is_an_authorized_empty_scope_not_a_refusal() {
        // An agent-slot with no readable board lists as nothing (200 []), never 403.
        let mut ports = FakePorts::new();
        let access = resolve_task_list_boards(&agent_slot("agent-1"), None, &mut ports)
            .await
            .unwrap();
        assert!(authorized(access).is_empty());
    }

    #[tokio::test]
    async fn a_union_workspace_without_read_tasks_is_silently_dropped() {
        // The union FILTERS by capability — an unreadable workspace leaves an empty
        // authorized scope, still a 200 [] rather than a refusal.
        let mut ports = FakePorts::new();
        ports.readable = vec![ws_board("ws-1")];
        ports.read_tasks = false;
        let access = resolve_task_list_boards(&agent_slot("agent-1"), None, &mut ports)
            .await
            .unwrap();
        assert!(authorized(access).is_empty());
    }

    #[tokio::test]
    async fn a_conversation_frame_without_a_selector_reads_its_own_board() {
        let mut ports = FakePorts::new();
        ports.resolved_board = Some(pair_board("agent-1:user-1"));
        let access =
            resolve_task_list_boards(&conversation_frame("agent-1", "conv-1"), None, &mut ports)
                .await
                .unwrap();
        assert_eq!(authorized(access), vec![pair_board("agent-1:user-1")]);
        // The frame arm resolved from the frame's OWN conversation, not a selector.
        assert_eq!(
            ports.observed_source,
            Some(BoardScopeSource::FrameConversation(
                ConversationId::parse("conv-1").unwrap()
            ))
        );
    }

    #[tokio::test]
    async fn an_explicit_selector_overrides_the_frame_and_resolves_that_workspace() {
        let mut ports = FakePorts::new();
        ports.resolved_board = Some(ws_board("ws-9"));
        let selector = BoardScopeSource::SelectedWorkspace(WorkspaceId::parse("ws-9").unwrap());
        let access = resolve_task_list_boards(
            &conversation_frame("agent-1", "conv-1"),
            Some(&selector),
            &mut ports,
        )
        .await
        .unwrap();
        assert_eq!(authorized(access), vec![ws_board("ws-9")]);
        // The selector arm resolved the SELECTED workspace, ignoring the frame.
        assert_eq!(ports.observed_source, Some(selector));
    }

    #[tokio::test]
    async fn a_direct_pair_frame_is_admitted_without_a_capability_axis() {
        // A direct-pair board has no read_tasks axis; even with the capability flag off
        // the frame's own pair board is authorized (participation is the per-row gate).
        let mut ports = FakePorts::new();
        ports.resolved_board = Some(pair_board("agent-1:user-1"));
        ports.read_tasks = false;
        let access =
            resolve_task_list_boards(&conversation_frame("agent-1", "conv-1"), None, &mut ports)
                .await
                .unwrap();
        assert_eq!(authorized(access), vec![pair_board("agent-1:user-1")]);
    }
}
