use std::collections::HashSet;
use std::error::Error;

use silverhand_backend_core::conversation_messaging::messaging::task::{
    AgentBoardResolver, AgentDisplayNameReader, AgentEventNames, AgentReadableBoards, BoardKey,
    BoardResolution, BoardScopeRequest, BoardScopeSource, PairKey, ReadableBoard, ResolvedBoard,
    TaskByIdCapability, TaskId, TaskParticipationReads, TaskReadCapability, TaskWriteCapability,
    TaskWriteDoorReads, WorkspaceMembership,
};
use silverhand_backend_core::identity_access::agent_peer_profile::{
    CallerConversationPlacement, CallerReadMessagesCapability, CallerWorkspaceHorizon,
    WorkspacePlacement,
};
use silverhand_backend_core::identity_access::workspace_name::CanonicalWorkspaceName;
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        AddressRefusal, AddressedPrincipals, CapabilityDenied, ChannelAccess, ChannelAccessRefusal,
        DeliveryIntent, DirectLineAuthority, DirectPairAllocation, DirectPairOpenedPort,
        DirectPairParticipants, OpenDirectPairError, PrincipalIdentity, PrincipalKind,
        ResolvedStream, SendCapability, SendClock, SendEntropy, TargetAddress,
        TargetAddressOperatorReason, TargetAddressRefusalPort, TargetAddressRefusalReport,
        ThreadRootId,
        conversation_title::{
            fold_name_key, is_space_separator, name_character_is_forbidden, name_is_nfc,
        },
    },
    foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId},
    foundation::persistence::PersistenceFailure,
};
use sqlx::{Row, SqliteConnection};
use thiserror::Error;

use super::{
    ENDPOINT_KIND_AGENT, ENDPOINT_KIND_HUMAN, MessagingStore, MessagingStoreError,
    PLACEMENT_DIRECT_PAIR, SELECT_AGENT_PARTICIPATES, SELECT_AGENT_PARTICIPATING_SET,
    SELECT_USER_PARTICIPATES, driver,
};

const SELECT_DIRECT_PAIR_INITIATION_CANDIDATE: &str = "WITH candidate AS (\
     SELECT a.id,a.created_by_user_id,a.session_scope FROM agents a WHERE \
     CASE ? WHEN 'id' THEN a.id = ? WHEN 'name' THEN a.name = ? ELSE 0 END \
     AND a.materialization_status = 'active' AND a.deletion_started_at IS NULL\
     ) \
     SELECT id,created_by_user_id AS viewer_id,session_scope FROM candidate \
     WHERE created_by_user_id = ? \
     UNION ALL \
     SELECT candidate.id,CASE WHEN c.endpoint_a_kind = 'human' THEN c.endpoint_a_id \
     ELSE c.endpoint_b_id END AS viewer_id,candidate.session_scope \
     FROM candidate JOIN conversations c ON c.placement_kind = 'direct_pair' \
     AND c.endpoint_a_deleted_at IS NULL AND c.endpoint_b_deleted_at IS NULL \
     AND ((c.endpoint_a_kind = 'human' AND c.endpoint_a_id = ? \
       AND c.endpoint_b_kind = 'agent' AND c.endpoint_b_id = candidate.id) \
      OR (c.endpoint_b_kind = 'human' AND c.endpoint_b_id = ? \
       AND c.endpoint_a_kind = 'agent' AND c.endpoint_a_id = candidate.id)) \
     UNION ALL \
     SELECT candidate.id,wu.user_id,candidate.session_scope \
     FROM candidate JOIN workspace_members wm ON wm.agent_id = candidate.id \
     JOIN workspace_users wu ON wu.workspace_id = wm.workspace_id AND wu.user_id = ? \
     LIMIT 1";

pub(crate) enum DirectPairInitiationTarget<'value> {
    Id(&'value str),
    Name(&'value str),
}

pub(crate) struct DirectPairInitiationCandidate {
    pub(crate) viewer: UserId,
    pub(crate) target: AgentId,
    pub(crate) scope:
        silverhand_backend_core::identity_access::agents::session_scope::AgentSessionScope,
}

pub(crate) async fn direct_pair_initiation_candidate(
    pool: &crate::foundation::database::SqliteDatabase,
    viewer: &UserId,
    target: DirectPairInitiationTarget<'_>,
) -> Result<Option<DirectPairInitiationCandidate>, PersistenceFailure> {
    let (selector, raw) = match target {
        DirectPairInitiationTarget::Id(raw) => ("id", raw),
        DirectPairInitiationTarget::Name(raw) => ("name", raw),
    };
    let row = sqlx::query(SELECT_DIRECT_PAIR_INITIATION_CANDIDATE)
        .bind(selector)
        .bind(raw)
        .bind(raw)
        .bind(viewer.as_str())
        .bind(viewer.as_str())
        .bind(viewer.as_str())
        .bind(viewer.as_str())
        .fetch_optional(pool)
        .await
        .map_err(driver)?;
    row.map(|row| {
        let source_viewer: String = row.try_get("viewer_id").map_err(driver)?;
        let source_target: String = row.try_get("id").map_err(driver)?;
        let scope: String = row.try_get("session_scope").map_err(driver)?;
        Ok(DirectPairInitiationCandidate {
            viewer: UserId::parse(source_viewer).map_err(|_| corrupt("viewer_id"))?,
            target: AgentId::parse(source_target).map_err(|_| corrupt("agents.id"))?,
            scope: silverhand_backend_core::identity_access::agents::session_scope::AgentSessionScope::parse(&scope)
                .ok_or_else(|| corrupt("agents.session_scope"))?,
        })
    })
    .transpose()
}

const PLACEMENT_WORKSPACE: &str = "workspace";
const ADDRESS_ID_LEN: usize = 16;

#[derive(Debug, Error)]
pub enum TargetAddressStoreError<O, R>
where
    O: Error + Send + Sync + 'static,
    R: Error + Send + Sync + 'static,
{
    #[error("target lookup failed")]
    Persistence(#[source] PersistenceFailure),
    #[error("target allocation clock failed")]
    Clock(
        #[source]
        silverhand_backend_core::conversation_messaging::messaging::EnvironmentUnavailable,
    ),
    #[error("direct-pair open failed")]
    Open(#[source] OpenDirectPairError<PersistenceFailure, O>),
    #[error("target refusal observation failed")]
    Observation(#[source] R),
}

fn corrupt(field: &'static str) -> PersistenceFailure {
    PersistenceFailure::from_source(MessagingStoreError::MalformedIdentity { field })
}

#[derive(Clone)]
pub struct SqliteTargetAddress<E, O, R> {
    store: MessagingStore<E>,
    opened: O,
    refusals: R,
}

impl<E, O, R> SqliteTargetAddress<E, O, R> {
    pub(super) fn new(store: MessagingStore<E>, opened: O, refusals: R) -> Self {
        Self {
            store,
            opened,
            refusals,
        }
    }
}

struct RejectedTarget {
    refusal: AddressRefusal,
    reason: TargetAddressOperatorReason,
}

impl RejectedTarget {
    const fn new(refusal: AddressRefusal, reason: TargetAddressOperatorReason) -> Self {
        Self { refusal, reason }
    }
}

pub(crate) struct ConversationPlacement {
    pub(crate) id: ConversationId,
    pub(crate) kind: String,
    workspace: Option<WorkspaceId>,
    endpoint_a: Option<(PrincipalIdentity, bool)>,
    endpoint_b: Option<(PrincipalIdentity, bool)>,
}

/// One connection-bound spelling of the conversation placement, participation
/// and direct-pair liveness authorities. Read-only operations can place this
/// reader over a transaction so all three answers come from one snapshot;
/// existing point consumers use the same decoder and predicates without
/// copying their SQL.
pub(crate) struct ConversationAuthorityReader<'connection> {
    connection: &'connection mut SqliteConnection,
}

impl<'connection> ConversationAuthorityReader<'connection> {
    pub(crate) fn new(connection: &'connection mut SqliteConnection) -> Self {
        Self { connection }
    }

    pub(crate) async fn conversation(
        &mut self,
        id: &ConversationId,
    ) -> Result<Option<ConversationPlacement>, PersistenceFailure> {
        let row = sqlx::query(
            "SELECT id, placement_kind, workspace_id, endpoint_a_kind, endpoint_a_id, \
             endpoint_a_deleted_at, endpoint_b_kind, endpoint_b_id, endpoint_b_deleted_at \
             FROM conversations WHERE id = ?",
        )
        .bind(id.as_str())
        .fetch_optional(&mut *self.connection)
        .await
        .map_err(driver)?;
        row.map(decode_placement).transpose()
    }

    pub(crate) async fn agent_participation_source(
        &mut self,
        conversation: &ConversationPlacement,
        agent: &AgentId,
    ) -> Result<Option<AgentId>, PersistenceFailure> {
        match conversation.kind.as_str() {
            PLACEMENT_WORKSPACE => {
                let source: Option<String> = sqlx::query_scalar(
                    "SELECT agent_id FROM conversation_agents \
                     WHERE conversation_id = ? AND agent_id = ?",
                )
                .bind(conversation.id.as_str())
                .bind(agent.as_str())
                .fetch_optional(&mut *self.connection)
                .await
                .map_err(driver)?;
                source
                    .map(AgentId::parse)
                    .transpose()
                    .map_err(|_| corrupt("conversation_agents.agent_id"))
            }
            PLACEMENT_DIRECT_PAIR => Ok(conversation
                .endpoint_a
                .as_ref()
                .into_iter()
                .chain(conversation.endpoint_b.as_ref())
                .find_map(|(principal, deleted)| match principal {
                    PrincipalIdentity::Agent(source) if !deleted && source == agent => {
                        Some(source.clone())
                    }
                    _ => None,
                })),
            _ => Err(corrupt("conversations.placement_kind")),
        }
    }

    pub(crate) async fn accepts_posts(
        &mut self,
        conversation: &ConversationPlacement,
    ) -> Result<bool, PersistenceFailure> {
        match conversation.kind.as_str() {
            PLACEMENT_WORKSPACE => Ok(true),
            PLACEMENT_DIRECT_PAIR => {
                let endpoints = [
                    conversation
                        .endpoint_a
                        .as_ref()
                        .ok_or_else(|| corrupt("conversations.endpoint_a"))?,
                    conversation
                        .endpoint_b
                        .as_ref()
                        .ok_or_else(|| corrupt("conversations.endpoint_b"))?,
                ];
                for (principal, deleted) in endpoints {
                    if *deleted || !self.principal_allows_direct_messages(principal).await? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            _ => Err(corrupt("conversations.placement_kind")),
        }
    }

    async fn principal_allows_direct_messages(
        &mut self,
        principal: &PrincipalIdentity,
    ) -> Result<bool, PersistenceFailure> {
        let found: Option<i64> = match principal.kind() {
            PrincipalKind::Agent => {
                sqlx::query_scalar("SELECT 1 FROM agents WHERE id = ? AND session_scope = 'agent'")
                    .bind(principal.as_str())
                    .fetch_optional(&mut *self.connection)
                    .await
                    .map_err(driver)?
            }
            PrincipalKind::Human => sqlx::query_scalar("SELECT 1 FROM users WHERE id = ?")
                .bind(principal.as_str())
                .fetch_optional(&mut *self.connection)
                .await
                .map_err(driver)?,
        };
        Ok(found.is_some())
    }
}

/// Three-valued workspace membership for a task-write door: `Absent` (no such
/// workspace → 404), `NotMember` (present but the agent may not target it → 403),
/// `Member`. Mirrors the read board's 404-vs-403 split.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentWorkspaceMembership {
    Absent,
    NotMember,
    Member,
}

/// The connection-bound authority a task-WRITE door consults BEFORE the write
/// transaction (matching TS, which resolves workspace membership + the
/// `write_tasks`/`create_tasks`/`read_tasks` capability pre-tx). It reads the same
/// `workspaces`/`workspace_members`/`workspace_agent_capabilities` tables the read
/// board and send gate already own, through the shared parameterized
/// [`capability_predicate`] — each task capability named by its own token, never
/// borrowed. It touches no `tasks` row: authorization is a fact about the workspace,
/// not the task.
pub(crate) struct TaskWriteAuthorityReader<'connection> {
    connection: &'connection mut SqliteConnection,
}

impl<'connection> TaskWriteAuthorityReader<'connection> {
    pub(crate) fn new(connection: &'connection mut SqliteConnection) -> Self {
        Self { connection }
    }

    /// Resolve the agent's membership of a workspace board (`Absent`/`NotMember`/
    /// `Member`) — an agent-home workspace admits only its owner, every other its
    /// `workspace_members` rows. The parity of `isAgentWorkspaceMember` folded with
    /// the route's `workspaces.get` existence check into one read (no check-then-act
    /// gap).
    pub(crate) async fn agent_workspace_membership(
        &mut self,
        workspace: &WorkspaceId,
        agent: &AgentId,
    ) -> Result<AgentWorkspaceMembership, PersistenceFailure> {
        let verdict: Option<i64> = sqlx::query_scalar(AGENT_WORKSPACE_MEMBERSHIP)
            .bind(agent.as_str())
            .bind(agent.as_str())
            .bind(workspace.as_str())
            .fetch_optional(&mut *self.connection)
            .await
            .map_err(driver)?;
        Ok(match verdict {
            None => AgentWorkspaceMembership::Absent,
            Some(0) => AgentWorkspaceMembership::NotMember,
            Some(_) => AgentWorkspaceMembership::Member,
        })
    }

    async fn has_capability(
        &mut self,
        capability: &'static str,
        workspace: &WorkspaceId,
        agent: &AgentId,
    ) -> Result<bool, PersistenceFailure> {
        crate::foundation::capability::agent_has_workspace_capability(
            &mut *self.connection,
            workspace.as_str(),
            agent.as_str(),
            capability,
        )
        .await
        .map_err(driver)
    }

    /// `write_tasks` on a workspace board (owner arm ∨ explicit grant).
    pub(crate) async fn may_write_tasks(
        &mut self,
        workspace: &WorkspaceId,
        agent: &AgentId,
    ) -> Result<bool, PersistenceFailure> {
        self.has_capability("write_tasks", workspace, agent).await
    }

    /// `create_tasks` on a workspace board.
    pub(crate) async fn may_create_tasks(
        &mut self,
        workspace: &WorkspaceId,
        agent: &AgentId,
    ) -> Result<bool, PersistenceFailure> {
        self.has_capability("create_tasks", workspace, agent).await
    }

    /// `read_tasks` on a workspace board — the edit door needs it in addition to
    /// `write_tasks`, because an edit returns the full task definition.
    pub(crate) async fn may_read_tasks(
        &mut self,
        workspace: &WorkspaceId,
        agent: &AgentId,
    ) -> Result<bool, PersistenceFailure> {
        self.has_capability("read_tasks", workspace, agent).await
    }

    /// The workspaces a canonical name resolves to, in the TS `findAllByName`
    /// DETERMINISTIC order (agent-home workspaces after regular ones, then
    /// `created_at ASC`) — the explicit `ORDER BY` never relies on SQLite's
    /// undeclared row order. A regular workspace matches by `name_key`, an
    /// agent-home one by its raw `name` (it has no `name_key`); the two branches
    /// mirror the store shape. The caller decides ambiguity (`len > 1`) BEFORE any
    /// membership read, so a candidate count cannot be rewritten by a later
    /// authorization result.
    pub(crate) async fn find_workspaces_by_name(
        &mut self,
        canonical: &CanonicalWorkspaceName,
    ) -> Result<Vec<WorkspaceId>, PersistenceFailure> {
        let rows = sqlx::query(FIND_WORKSPACES_BY_NAME)
            .bind(canonical.key())
            .bind(canonical.name())
            .fetch_all(&mut *self.connection)
            .await
            .map_err(driver)?;
        rows.into_iter()
            .map(|row| {
                let id: String = row.try_get("id").map_err(driver)?;
                WorkspaceId::parse(id).map_err(|_| corrupt("workspaces.id"))
            })
            .collect()
    }

    /// The conversation's placement-derived board (`BoardKey`) for the no-selector
    /// arm, via the shared [`CONVERSATION_BOARD_PARTS`] decode on this connection.
    /// `None` = no such conversation (the caller answers "no nameable board"); a
    /// present row's placement is the closed v76 set, so a missing/unparseable
    /// coordinate on it is schema corruption (safe-500), never a denial.
    pub(crate) async fn conversation_board(
        &mut self,
        conversation: &ConversationId,
    ) -> Result<Option<BoardKey>, PersistenceFailure> {
        let Some(row) = sqlx::query(CONVERSATION_BOARD_PARTS)
            .bind(conversation.as_str())
            .fetch_optional(&mut *self.connection)
            .await
            .map_err(driver)?
        else {
            return Ok(None);
        };
        let placement: String = row.try_get("placement_kind").map_err(driver)?;
        let board = match placement.as_str() {
            PLACEMENT_WORKSPACE => {
                let workspace: Option<String> = row.try_get("workspace_id").map_err(driver)?;
                let workspace = workspace.ok_or_else(|| corrupt("conversations.workspace_id"))?;
                BoardKey::Workspace(
                    WorkspaceId::parse(workspace)
                        .map_err(|_| corrupt("conversations.workspace_id"))?,
                )
            }
            PLACEMENT_DIRECT_PAIR => {
                let pair: Option<String> = row.try_get("pair_key").map_err(driver)?;
                let pair = pair.ok_or_else(|| corrupt("conversations.pair_key"))?;
                BoardKey::Pair(PairKey::parse(pair).map_err(|_| corrupt("conversations.pair_key"))?)
            }
            _ => return Err(corrupt("conversations.placement_kind")),
        };
        Ok(Some(board))
    }
}

impl<E> MessagingStore<E> {
    pub(crate) async fn authority_conversation(
        &self,
        id: &ConversationId,
    ) -> Result<Option<ConversationPlacement>, PersistenceFailure> {
        let mut connection = self.pool.acquire().await.map_err(driver)?;
        ConversationAuthorityReader::new(&mut connection)
            .conversation(id)
            .await
    }

    async fn agent_participates(
        &self,
        conversation: &ConversationPlacement,
        agent: &AgentId,
    ) -> Result<bool, PersistenceFailure> {
        let mut connection = self.pool.acquire().await.map_err(driver)?;
        Ok(ConversationAuthorityReader::new(&mut connection)
            .agent_participation_source(conversation, agent)
            .await?
            .is_some())
    }

    async fn accepts_posts(
        &self,
        conversation: &ConversationPlacement,
    ) -> Result<bool, PersistenceFailure> {
        let mut connection = self.pool.acquire().await.map_err(driver)?;
        ConversationAuthorityReader::new(&mut connection)
            .accepts_posts(conversation)
            .await
    }

    async fn resolve_thread(
        &self,
        conversation: ConversationId,
        thread: Option<String>,
    ) -> Result<Result<ResolvedStream, RejectedTarget>, PersistenceFailure> {
        let Some(thread) = thread else {
            return Ok(Ok(ResolvedStream {
                conversation,
                thread_root: None,
            }));
        };
        let row: Option<Option<String>> = sqlx::query_scalar(
            "SELECT thread_root_id FROM messages WHERE conversation_id = ? AND id = ?",
        )
        .bind(conversation.as_str())
        .bind(&thread)
        .fetch_optional(&self.pool)
        .await
        .map_err(driver)?;
        match row {
            None => {
                return Ok(Err(RejectedTarget::new(
                    AddressRefusal::ThreadNotInTarget,
                    TargetAddressOperatorReason::ThreadRootNotFound,
                )));
            }
            Some(Some(_)) => {
                return Ok(Err(RejectedTarget::new(
                    AddressRefusal::ThreadNotInTarget,
                    TargetAddressOperatorReason::ThreadRootNotARoot,
                )));
            }
            Some(None) => {}
        }
        let root = ThreadRootId::parse(thread).map_err(|_| corrupt("messages.id"))?;
        Ok(Ok(ResolvedStream {
            conversation,
            thread_root: Some(root),
        }))
    }
}

impl<E> ChannelAccess for MessagingStore<E>
where
    E: Send + Sync,
{
    type Error = PersistenceFailure;

    async fn may_post(
        &self,
        channel: &ConversationId,
        author: &UserId,
    ) -> Result<Result<(), ChannelAccessRefusal>, Self::Error> {
        let Some(conversation) = self.authority_conversation(channel).await? else {
            return Ok(Err(ChannelAccessRefusal::NotFound));
        };
        let allowed = match conversation.kind.as_str() {
            PLACEMENT_WORKSPACE => {
                let workspace = conversation
                    .workspace
                    .as_ref()
                    .ok_or_else(|| corrupt("conversations.workspace_id"))?;
                let present: Option<i64> = sqlx::query_scalar(
                    "SELECT 1 FROM conversation_users cu JOIN workspace_users wu \
                     ON wu.workspace_id = ? AND wu.user_id = cu.user_id \
                     WHERE cu.conversation_id = ? AND cu.user_id = ?",
                )
                .bind(workspace.as_str())
                .bind(channel.as_str())
                .bind(author.as_str())
                .fetch_optional(&self.pool)
                .await
                .map_err(driver)?;
                present.is_some()
            }
            PLACEMENT_DIRECT_PAIR => {
                let a = conversation
                    .endpoint_a
                    .as_ref()
                    .ok_or_else(|| corrupt("conversations.endpoint_a"))?;
                let b = conversation
                    .endpoint_b
                    .as_ref()
                    .ok_or_else(|| corrupt("conversations.endpoint_b"))?;
                let participates = a.0 == PrincipalIdentity::Human(author.clone())
                    || b.0 == PrincipalIdentity::Human(author.clone());
                participates && self.accepts_posts(&conversation).await?
            }
            _ => return Err(corrupt("conversations.placement_kind")),
        };
        Ok(if allowed {
            Ok(())
        } else {
            Err(ChannelAccessRefusal::Forbidden)
        })
    }
}

impl<E> SendCapability for MessagingStore<E>
where
    E: Send + Sync,
{
    type Error = PersistenceFailure;

    async fn may_send_messages(
        &self,
        agent: &AgentId,
        conversation: &ConversationId,
    ) -> Result<Result<(), CapabilityDenied>, Self::Error> {
        let Some(place) = self.authority_conversation(conversation).await? else {
            return Ok(Err(CapabilityDenied));
        };
        let allowed = match place.kind.as_str() {
            PLACEMENT_WORKSPACE => {
                let workspace = place
                    .workspace
                    .as_ref()
                    .ok_or_else(|| corrupt("conversations.workspace_id"))?;
                crate::foundation::capability::agent_has_workspace_capability(
                    &self.pool,
                    workspace.as_str(),
                    agent.as_str(),
                    "send_messages",
                )
                .await
                .map_err(driver)?
            }
            PLACEMENT_DIRECT_PAIR => {
                self.agent_participates(&place, agent).await? && self.accepts_posts(&place).await?
            }
            _ => return Err(corrupt("conversations.placement_kind")),
        };
        Ok(if allowed {
            Ok(())
        } else {
            Err(CapabilityDenied)
        })
    }
}

impl<E> DirectLineAuthority for MessagingStore<E>
where
    E: Send + Sync,
{
    async fn direct_lines(
        &self,
        author: &UserId,
        addressed: &AddressedPrincipals,
    ) -> Result<Vec<AgentId>, PersistenceFailure> {
        let mut agents = Vec::new();
        for reference in addressed
            .iter()
            .filter(|r| r.kind() == PrincipalKind::Agent)
        {
            let candidate = direct_pair_initiation_candidate(
                &self.pool,
                author,
                DirectPairInitiationTarget::Name(reference.handle().as_str()),
            )
            .await?;
            if let Some(candidate) = candidate.filter(|candidate| {
                candidate.scope
                    == silverhand_backend_core::identity_access::agents::session_scope::AgentSessionScope::Agent
            }) {
                agents.push(candidate.target);
            }
        }
        Ok(agents)
    }
}

impl<E, O, R> SqliteTargetAddress<E, O, R>
where
    E: SendClock + SendEntropy + Send + Sync,
    O: DirectPairOpenedPort + Send + Sync,
    R: TargetAddressRefusalPort + Send + Sync,
{
    async fn resolve_unobserved(
        &self,
        agent: &AgentId,
        target: &str,
        allocation: DirectPairAllocation,
        intent: DeliveryIntent,
    ) -> Result<Result<ResolvedStream, RejectedTarget>, TargetAddressStoreError<O::Error, R::Error>>
    {
        let Some(parsed) = parse_target(target) else {
            return Ok(Err(RejectedTarget::new(
                AddressRefusal::Malformed,
                TargetAddressOperatorReason::Grammar,
            )));
        };
        let (conversation, thread) = match parsed {
            ParsedTarget::Conversation { id, thread } => {
                let id = ConversationId::parse(id).map_err(|_| {
                    TargetAddressStoreError::Persistence(corrupt("target.conversation_id"))
                })?;
                let Some(place) = self
                    .store
                    .authority_conversation(&id)
                    .await
                    .map_err(TargetAddressStoreError::Persistence)?
                else {
                    return Ok(Err(RejectedTarget::new(
                        AddressRefusal::Unresolved,
                        TargetAddressOperatorReason::ConversationNotFound,
                    )));
                };
                if !self
                    .store
                    .agent_participates(&place, agent)
                    .await
                    .map_err(TargetAddressStoreError::Persistence)?
                {
                    return Ok(Err(RejectedTarget::new(
                        AddressRefusal::Unresolved,
                        TargetAddressOperatorReason::NotParticipant,
                    )));
                }
                (id, thread)
            }
            ParsedTarget::Named {
                workspace_key,
                title_key,
                thread,
            } => {
                let roots: Vec<String> = sqlx::query_scalar(
                    "SELECT id FROM workspaces WHERE owner_agent_id IS NULL AND name_key = ? LIMIT 2",
                )
                .bind(workspace_key)
                .fetch_all(&self.store.pool)
                .await
                .map_err(driver)
                .map_err(TargetAddressStoreError::Persistence)?;
                let workspace = match roots.as_slice() {
                    [] => {
                        return Ok(Err(RejectedTarget::new(
                            AddressRefusal::Unresolved,
                            TargetAddressOperatorReason::WorkspaceNotFound,
                        )));
                    }
                    [raw] => WorkspaceId::parse(raw.clone()).map_err(|_| {
                        TargetAddressStoreError::Persistence(corrupt("workspaces.id"))
                    })?,
                    _ => {
                        return Ok(Err(RejectedTarget::new(
                            AddressRefusal::Unresolved,
                            TargetAddressOperatorReason::WorkspaceAmbiguous,
                        )));
                    }
                };
                let rows: Vec<String> = sqlx::query_scalar(
                    "SELECT id FROM conversations WHERE workspace_id = ? AND title_key = ? LIMIT 2",
                )
                .bind(workspace.as_str())
                .bind(title_key)
                .fetch_all(&self.store.pool)
                .await
                .map_err(driver)
                .map_err(TargetAddressStoreError::Persistence)?;
                let raw = match rows.as_slice() {
                    [] => {
                        return Ok(Err(RejectedTarget::new(
                            AddressRefusal::Unresolved,
                            TargetAddressOperatorReason::TitleNotFound,
                        )));
                    }
                    [raw] => raw.clone(),
                    _ => {
                        return Err(TargetAddressStoreError::Persistence(corrupt(
                            "target.named_stream",
                        )));
                    }
                };
                let id = addressable_conversation(raw, "conversations.id")
                    .map_err(TargetAddressStoreError::Persistence)?;
                let place = self
                    .store
                    .authority_conversation(&id)
                    .await
                    .map_err(TargetAddressStoreError::Persistence)?
                    .ok_or_else(|| {
                        TargetAddressStoreError::Persistence(corrupt("conversations.id"))
                    })?;
                if !self
                    .store
                    .agent_participates(&place, agent)
                    .await
                    .map_err(TargetAddressStoreError::Persistence)?
                {
                    return Ok(Err(RejectedTarget::new(
                        AddressRefusal::Unresolved,
                        TargetAddressOperatorReason::NotParticipant,
                    )));
                }
                (id, thread)
            }
            ParsedTarget::DirectPair { handle, thread } => {
                let agent_row: Option<String> =
                    sqlx::query_scalar("SELECT id FROM agents WHERE name = ?")
                        .bind(&handle)
                        .fetch_optional(&self.store.pool)
                        .await
                        .map_err(driver)
                        .map_err(TargetAddressStoreError::Persistence)?;
                let user_row: Option<String> =
                    sqlx::query_scalar("SELECT id FROM users WHERE name = ?")
                        .bind(&handle)
                        .fetch_optional(&self.store.pool)
                        .await
                        .map_err(driver)
                        .map_err(TargetAddressStoreError::Persistence)?;
                let other = match (agent_row, user_row) {
                    (Some(_), Some(_)) => {
                        return Err(TargetAddressStoreError::Persistence(corrupt(
                            "principal.handle",
                        )));
                    }
                    (Some(raw), None) => {
                        PrincipalIdentity::Agent(AgentId::parse(raw).map_err(|_| {
                            TargetAddressStoreError::Persistence(corrupt("agents.id"))
                        })?)
                    }
                    (None, Some(raw)) => {
                        PrincipalIdentity::Human(UserId::parse(raw).map_err(|_| {
                            TargetAddressStoreError::Persistence(corrupt("users.id"))
                        })?)
                    }
                    (None, None) => {
                        return Ok(Err(RejectedTarget::new(
                            AddressRefusal::Unresolved,
                            TargetAddressOperatorReason::UnknownHandle,
                        )));
                    }
                };
                let me = PrincipalIdentity::Agent(agent.clone());
                if other == me {
                    return Ok(Err(RejectedTarget::new(
                        AddressRefusal::SelfPair,
                        TargetAddressOperatorReason::SelfPair,
                    )));
                }
                // A scope hint is target information: disclose it only through the
                // existing peer-profile workspace horizon + read_messages authority.
                // Reads and invisible handles retain their opaque resolution contract.
                if intent == DeliveryIntent::Write
                    && let PrincipalIdentity::Agent(peer) = &other
                    && self
                        .store
                        .visible_peer_dm_scope_refusal(agent, peer)
                        .await
                        .map_err(TargetAddressStoreError::Persistence)?
                {
                    return Ok(Err(RejectedTarget::new(
                        AddressRefusal::DirectMessageScope,
                        TargetAddressOperatorReason::DirectMessageScope,
                    )));
                }
                let participants = DirectPairParticipants::between(me, other).map_err(|_| {
                    TargetAddressStoreError::Persistence(corrupt("direct_pair.participants"))
                })?;
                let existing: Option<String> = sqlx::query_scalar(super::SELECT_DIRECT_PAIR_ID)
                    .bind(super::principal_kind(participants.first()))
                    .bind(participants.first().as_str())
                    .bind(super::principal_kind(participants.second()))
                    .bind(participants.second().as_str())
                    .fetch_optional(&self.store.pool)
                    .await
                    .map_err(driver)
                    .map_err(TargetAddressStoreError::Persistence)?;
                let id = match existing {
                    Some(raw) => addressable_conversation(raw, "conversations.id")
                        .map_err(TargetAddressStoreError::Persistence)?,
                    None if thread.is_some() || allocation == DirectPairAllocation::Forbidden => {
                        return Ok(Err(RejectedTarget::new(
                            AddressRefusal::Unresolved,
                            TargetAddressOperatorReason::DirectPairNotFound,
                        )));
                    }
                    None => {
                        for principal in [participants.first(), participants.second()] {
                            if let PrincipalIdentity::Agent(id) = principal {
                                let scope: Option<String> =
                                    sqlx::query_scalar(super::SELECT_AGENT_SESSION_SCOPE)
                                        .bind(id.as_str())
                                        .fetch_optional(&self.store.pool)
                                        .await
                                        .map_err(driver)
                                        .map_err(TargetAddressStoreError::Persistence)?;
                                if scope.as_deref() != Some("agent") {
                                    return Ok(Err(RejectedTarget::new(
                                        AddressRefusal::Unresolved,
                                        TargetAddressOperatorReason::DirectPairNotFound,
                                    )));
                                }
                            }
                        }
                        self.store
                            .open_direct_pair(
                                &self.opened,
                                participants,
                                self.store
                                    .environment
                                    .now_unix_ms()
                                    .map_err(TargetAddressStoreError::Clock)?,
                            )
                            .await
                            .map_err(TargetAddressStoreError::Open)?
                    }
                };
                (id, thread)
            }
        };
        self.store
            .resolve_thread(conversation, thread)
            .await
            .map_err(TargetAddressStoreError::Persistence)
    }
}

impl<E, O, R> TargetAddress for SqliteTargetAddress<E, O, R>
where
    E: SendClock + SendEntropy + Send + Sync,
    O: DirectPairOpenedPort + Send + Sync,
    R: TargetAddressRefusalPort + Send + Sync,
{
    type Error = TargetAddressStoreError<O::Error, R::Error>;

    async fn resolve(
        &self,
        agent: &AgentId,
        target: &str,
        allocation: DirectPairAllocation,
        intent: DeliveryIntent,
    ) -> Result<Result<ResolvedStream, AddressRefusal>, Self::Error> {
        match self
            .resolve_unobserved(agent, target, allocation, intent)
            .await?
        {
            Ok(stream) => Ok(Ok(stream)),
            Err(rejected) => {
                self.refusals
                    .refused(&TargetAddressRefusalReport::new(
                        rejected.refusal,
                        rejected.reason,
                        target,
                        agent.clone(),
                        intent,
                    ))
                    .await
                    .map_err(TargetAddressStoreError::Observation)?;
                Ok(Err(rejected.refusal))
            }
        }
    }
}

fn decode_placement(
    row: sqlx::sqlite::SqliteRow,
) -> Result<ConversationPlacement, PersistenceFailure> {
    let id: String = row.try_get("id").map_err(driver)?;
    let kind: String = row.try_get("placement_kind").map_err(driver)?;
    let workspace: Option<String> = row.try_get("workspace_id").map_err(driver)?;
    let endpoint_a = decode_endpoint(
        &row,
        "endpoint_a_kind",
        "endpoint_a_id",
        "endpoint_a_deleted_at",
    )?;
    let endpoint_b = decode_endpoint(
        &row,
        "endpoint_b_kind",
        "endpoint_b_id",
        "endpoint_b_deleted_at",
    )?;
    let id = ConversationId::parse(id).map_err(|_| corrupt("conversations.id"))?;
    let workspace = workspace
        .map(WorkspaceId::parse)
        .transpose()
        .map_err(|_| corrupt("conversations.workspace_id"))?;
    match kind.as_str() {
        PLACEMENT_WORKSPACE
            if workspace.is_some() && endpoint_a.is_none() && endpoint_b.is_none() => {}
        PLACEMENT_DIRECT_PAIR
            if workspace.is_none() && endpoint_a.is_some() && endpoint_b.is_some() => {}
        PLACEMENT_WORKSPACE | PLACEMENT_DIRECT_PAIR => {
            return Err(corrupt("conversations.placement"));
        }
        _ => return Err(corrupt("conversations.placement_kind")),
    }
    Ok(ConversationPlacement {
        id,
        kind,
        workspace,
        endpoint_a,
        endpoint_b,
    })
}

fn decode_endpoint(
    row: &sqlx::sqlite::SqliteRow,
    kind_field: &'static str,
    id_field: &'static str,
    deleted_field: &'static str,
) -> Result<Option<(PrincipalIdentity, bool)>, PersistenceFailure> {
    let kind: Option<String> = row.try_get(kind_field).map_err(driver)?;
    let id: Option<String> = row.try_get(id_field).map_err(driver)?;
    let deleted: Option<i64> = row.try_get(deleted_field).map_err(driver)?;
    match (kind.as_deref(), id) {
        (None, None) if deleted.is_none() => Ok(None),
        (Some(ENDPOINT_KIND_AGENT), Some(raw)) => Ok(Some((
            PrincipalIdentity::Agent(AgentId::parse(raw).map_err(|_| corrupt(id_field))?),
            deleted.is_some(),
        ))),
        (Some(ENDPOINT_KIND_HUMAN), Some(raw)) => Ok(Some((
            PrincipalIdentity::Human(UserId::parse(raw).map_err(|_| corrupt(id_field))?),
            deleted.is_some(),
        ))),
        _ => Err(corrupt(kind_field)),
    }
}

fn addressable_conversation(
    raw: String,
    field: &'static str,
) -> Result<ConversationId, PersistenceFailure> {
    if !address_id(&raw) {
        return Err(corrupt(field));
    }
    ConversationId::parse(raw).map_err(|_| corrupt(field))
}

enum ParsedTarget {
    Conversation {
        id: String,
        thread: Option<String>,
    },
    Named {
        workspace_key: String,
        title_key: String,
        thread: Option<String>,
    },
    DirectPair {
        handle: String,
        thread: Option<String>,
    },
}

fn parse_target(raw: &str) -> Option<ParsedTarget> {
    if let Some(rest) = raw.strip_prefix("#conv:")
        && let Some((id, thread)) = split_thread(rest, false)
        && address_id(id)
    {
        return Some(ParsedTarget::Conversation {
            id: id.into(),
            thread,
        });
    }
    if let Some(rest) = raw.strip_prefix("dm:@") {
        let (handle, thread) = split_thread(rest, false)?;
        let valid = handle.len() <= 64
            && handle
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphabetic)
            && handle
                .as_bytes()
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-');
        return valid.then(|| ParsedTarget::DirectPair {
            handle: handle.into(),
            thread,
        });
    }
    let rest = raw.strip_prefix('#')?;
    let (head, thread) = split_thread(rest, true)?;
    let (workspace, title) = head.split_once('/')?;
    if title.contains('/') {
        return None;
    }
    let workspace = decode_name_segment(workspace, 64)?;
    let title = decode_name_segment(title, 128)?;
    Some(ParsedTarget::Named {
        workspace_key: fold_name_key(&workspace),
        title_key: fold_name_key(&title),
        thread,
    })
}

fn split_thread(raw: &str, named: bool) -> Option<(&str, Option<String>)> {
    let Some((head, suffix)) = raw.rsplit_once("/thread:") else {
        return Some((raw, None));
    };
    if named && !head.contains('/') {
        return Some((raw, None));
    }
    address_id(suffix).then(|| (head, Some(suffix.to_owned())))
}

pub(crate) fn address_id(raw: &str) -> bool {
    raw.len() == ADDRESS_ID_LEN
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub(crate) fn encode_name_segment(raw: &str, max: usize) -> Option<String> {
    if raw.is_empty()
        || raw.chars().count() > max
        || raw.trim() != raw
        || !name_is_nfc(raw)
        || raw.chars().any(name_character_is_forbidden)
    {
        return None;
    }
    let mut encoded = String::new();
    for character in raw.chars() {
        match character {
            '%' => encoded.push_str("%25"),
            '/' => encoded.push_str("%2F"),
            ch if is_space_separator(ch) => {
                for byte in ch.to_string().as_bytes() {
                    use std::fmt::Write as _;
                    write!(&mut encoded, "%{byte:02X}").expect("writing to a String cannot fail");
                }
            }
            ch => encoded.push(ch),
        }
    }
    Some(encoded)
}

fn decode_name_segment(segment: &str, max: usize) -> Option<String> {
    if segment.is_empty() || segment.contains('/') {
        return None;
    }
    const ESCAPES: &[(&str, &str)] = &[
        ("%E3%80%80", "\u{3000}"),
        ("%E2%81%9F", "\u{205f}"),
        ("%E2%80%AF", "\u{202f}"),
        ("%E2%80%8A", "\u{200a}"),
        ("%E2%80%89", "\u{2009}"),
        ("%E2%80%88", "\u{2008}"),
        ("%E2%80%87", "\u{2007}"),
        ("%E2%80%86", "\u{2006}"),
        ("%E2%80%85", "\u{2005}"),
        ("%E2%80%84", "\u{2004}"),
        ("%E2%80%83", "\u{2003}"),
        ("%E2%80%82", "\u{2002}"),
        ("%E2%80%81", "\u{2001}"),
        ("%E2%80%80", "\u{2000}"),
        ("%E1%9A%80", "\u{1680}"),
        ("%C2%A0", "\u{00a0}"),
        ("%25", "%"),
        ("%2F", "/"),
        ("%20", " "),
    ];
    let mut decoded = String::new();
    let mut index = 0;
    while index < segment.len() {
        let rest = &segment[index..];
        if rest.starts_with('%') {
            let (escaped, value) = ESCAPES
                .iter()
                .find(|(escaped, _)| rest.starts_with(*escaped))?;
            decoded.push_str(value);
            index += escaped.len();
        } else {
            let ch = rest.chars().next()?;
            if ch == '%' || is_space_separator(ch) {
                return None;
            }
            decoded.push(ch);
            index += ch.len_utf8();
        }
    }
    if decoded.chars().count() > max
        || decoded.trim() != decoded
        || !name_is_nfc(&decoded)
        || decoded.chars().any(name_character_is_forbidden)
    {
        return None;
    }
    Some(decoded)
}

// --- Task-read authorities --------------------------------------------------
//
// The task bounded context defines three consumer ports for authorizing a task
// read (`backend_core::messaging::task::read`). All three are answered from the
// SAME roster and workspace-capability tables this store already owns for the
// send gate — participation from the placement roster, capability from the
// `owner_agent_id OR workspace_agent_capabilities` shape — so they live here
// beside `SendCapability` rather than in the `tasks` adapter, which reads only
// the `tasks` table. Nothing below touches `tasks`: an authorization is a fact
// about the conversation and the workspace, never about the task row.

use crate::foundation::capability::capability_predicate;

/// The `read_tasks` predicate — a named instance of the shared [`capability_predicate`].
macro_rules! read_tasks_predicate {
    () => {
        capability_predicate!("read_tasks")
    };
}

/// The set-shaped `read_tasks` read: `json_each(?)` drives the requested workspace set (one
/// bound JSON array, no dynamic SQL), each id joined to its workspace and probed against the
/// shared [`read_tasks_predicate`] tail. Returns the admitted SUBSET's ids; the core re-binds
/// each board against it, so the `json_each` set is a coordinate narrowing, never a witness.
const READABLE_TASK_WORKSPACES: &str = concat!(
    "SELECT w.id FROM json_each(?) requested JOIN workspaces w ON w.id = requested.value WHERE ",
    read_tasks_predicate!()
);

/// Agent membership of a workspace, three-valued so the caller can answer `404` vs
/// `403`: `NULL`/no row = no such workspace, `0` = the workspace exists but the agent
/// is not a member, `1` = a member. The parity of `isAgentWorkspaceMember` plus the
/// route's separate `workspaces.get` existence check, folded into ONE read so the
/// refusal path opens no check-then-act gap. An agent-home workspace (`owner_agent_id`
/// set) admits ONLY its owner; every other workspace admits its `workspace_members`
/// rows. The two arms are mutually exclusive on `owner_agent_id IS NULL`, so an
/// agent-home workspace never consults the members table and a plain member is never
/// mistaken for the owner. Membership is a DIFFERENT axis from [`READABLE_TASK_WORKSPACES`]:
/// membership decides which board namespace a selector may target; the capability
/// decides whether the member may read its tasks. A workspace board requires both.
const AGENT_WORKSPACE_MEMBERSHIP: &str = "SELECT CASE WHEN w.owner_agent_id = ? OR \
     (w.owner_agent_id IS NULL AND EXISTS (SELECT 1 FROM workspace_members wm \
     WHERE wm.workspace_id = w.id AND wm.agent_id = ?)) THEN 1 ELSE 0 END \
     FROM workspaces w WHERE w.id = ?";

/// Resolve a canonical workspace name to its board candidates, in the TS
/// `findAllByName` deterministic order: a regular workspace (`owner_agent_id IS
/// NULL`) matches `name_key`, an agent-home one (`owner_agent_id IS NOT NULL`)
/// matches its raw `name`; the explicit `ORDER BY` puts regular workspaces first
/// (`owner_agent_id IS NULL` → 0) then `created_at ASC`, so the candidate list is
/// deterministic and never leans on SQLite's undeclared row order.
const FIND_WORKSPACES_BY_NAME: &str = "SELECT w.id FROM workspaces w WHERE \
     (w.owner_agent_id IS NULL AND w.name_key = ?) \
     OR (w.owner_agent_id IS NOT NULL AND w.name = ?) \
     ORDER BY CASE WHEN w.owner_agent_id IS NULL THEN 0 ELSE 1 END, w.created_at ASC";

/// A conversation's placement-derived board coordinates: the placement kind and
/// the id each placement derives its board from. Mirrors `boardOfConversation`'s
/// authority (the v76 derivation trigger) but projects the RAW parts so the typed
/// [`BoardKey`] is built here rather than reparsed out of a `'ws:' || id` string.
/// An ABSENT conversation derives no board and honestly denies (`None`). A PRESENT
/// row is a committed fact of the v76 closed placement set (`workspace|direct_pair`),
/// each of which its shape obliges to carry its own coordinate: a missing, unknown,
/// or unparseable coordinate on a selected row is schema/projection corruption
/// (safe-500), never a scope denial.
const CONVERSATION_BOARD_PARTS: &str =
    "SELECT placement_kind, workspace_id, pair_key FROM conversations WHERE id = ?";

/// Every workspace board an agent may READ tasks on BEFORE the `read_tasks` fold — the
/// parity of `workspaces.listForAgent`'s two load-bearing arms: the non-agent-home
/// workspaces the agent joined (`workspace_members`, `owner_agent_id IS NULL`) UNIONed
/// with the agent's own agent-home workspace (`owner_agent_id = agent`). `UNION` (not
/// `UNION ALL`) so a workspace reachable by both arms is enumerated once. The `read_tasks`
/// capability is NOT applied here — [`authorize_task_list_boards`] folds it per distinct
/// board, matching the TS `.filter(canUseCapability('read_tasks'))` on this union.
const AGENT_READABLE_WORKSPACES: &str = "SELECT w.id FROM workspaces w \
     JOIN workspace_members wm ON wm.workspace_id = w.id \
     WHERE wm.agent_id = ? AND w.owner_agent_id IS NULL \
     UNION SELECT w.id FROM workspaces w WHERE w.owner_agent_id = ?";

/// Every direct-pair board an agent is an endpoint of — the parity of
/// `listAgentDirectPairBoardKeys`. A direct-pair board has no capability axis, so
/// membership (being an endpoint) is its whole read authority; the per-row participation
/// gate settles the rest downstream.
const AGENT_DIRECT_PAIR_BOARDS: &str = "SELECT pair_key FROM conversations \
     WHERE placement_kind = ? AND ((endpoint_a_kind = ? AND endpoint_a_id = ?) \
     OR (endpoint_b_kind = ? AND endpoint_b_id = ?))";

impl<E> TaskReadCapability for MessagingStore<E>
where
    E: Send + Sync,
{
    async fn may_read_tasks(
        &mut self,
        agent: &AgentId,
        workspace: &WorkspaceId,
    ) -> Result<bool, PersistenceFailure> {
        // The point form IS the set read over a set of one, so the two share a single
        // `read_tasks` predicate ([`READ_TASKS_PREDICATE`]) and can never drift.
        let readable = self
            .readable_task_workspaces(agent, std::slice::from_ref(workspace))
            .await?;
        Ok(readable.contains(workspace))
    }

    async fn readable_task_workspaces(
        &mut self,
        agent: &AgentId,
        workspaces: &[WorkspaceId],
    ) -> Result<HashSet<WorkspaceId>, PersistenceFailure> {
        // ONE round-trip for the whole distinct-board set: `w.id IN (...)` narrows the
        // coordinate, the shared `read_tasks` tail decides admission. An empty input asks
        // nothing. The returned ids are re-derived by matching the RETURNED strings back to
        // the input identities (never re-parsed), so the `IN` set is a coordinate narrowing
        // and a workspace the query omits is simply not admitted.
        if workspaces.is_empty() {
            return Ok(HashSet::new());
        }
        // The requested set is bound as ONE JSON array (`json_each` drives it); no dynamic
        // SQL. The two trailing binds are the agent for the owner arm and the capability
        // `EXISTS`. The returned ids are matched back to the input identities below, never
        // re-parsed, so the set is a coordinate narrowing.
        let requested = serde_json::to_string(
            &workspaces
                .iter()
                .map(WorkspaceId::as_str)
                .collect::<Vec<_>>(),
        )
        .map_err(|e| PersistenceFailure::from_source(MessagingStoreError::Serialization(e)))?;
        let rows = sqlx::query(READABLE_TASK_WORKSPACES)
            .bind(requested)
            .bind(agent.as_str())
            .bind(agent.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(driver)?;
        let mut admitted: HashSet<&str> = HashSet::new();
        for row in &rows {
            let id: &str = row.try_get("id").map_err(driver)?;
            admitted.insert(id);
        }
        // Project admission back onto the INPUT identities: result ⊆ input, no re-parse.
        Ok(workspaces
            .iter()
            .filter(|workspace| admitted.contains(workspace.as_str()))
            .cloned()
            .collect())
    }
}

impl<E> TaskWriteDoorReads for MessagingStore<E>
where
    E: Send + Sync,
{
    async fn may_mutate_task_by_id(
        &mut self,
        task: &TaskId,
        agent: &AgentId,
        required: TaskByIdCapability,
    ) -> Result<bool, PersistenceFailure> {
        let mut tx = self.pool.begin().await.map_err(driver)?;
        let conversation: Option<String> =
            sqlx::query_scalar("SELECT conversation_id FROM tasks WHERE id = ?")
                .bind(task.as_str())
                .fetch_optional(&mut *tx)
                .await
                .map_err(driver)?;
        let Some(conversation) = conversation else {
            return Ok(false);
        };
        let participates: Option<i64> = sqlx::query_scalar(SELECT_AGENT_PARTICIPATES)
            .bind(&conversation)
            .bind(agent.as_str())
            .bind(agent.as_str())
            .bind(agent.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(driver)?;
        if participates.is_none() {
            return Ok(false);
        }
        let placement: Option<(String, Option<String>)> =
            sqlx::query_as("SELECT placement_kind,workspace_id FROM conversations WHERE id = ?")
                .bind(&conversation)
                .fetch_optional(&mut *tx)
                .await
                .map_err(driver)?;
        let Some((kind, workspace)) = placement else {
            return Ok(false);
        };
        if kind == "direct_pair" {
            return Ok(true);
        }
        let Some(workspace) = (kind == "workspace").then_some(workspace).flatten() else {
            return Ok(false);
        };
        let workspace =
            WorkspaceId::parse(workspace).map_err(|_| corrupt("conversations.workspace_id"))?;
        let mut reader = TaskWriteAuthorityReader::new(&mut tx);
        if !reader.may_write_tasks(&workspace, agent).await? {
            return Ok(false);
        }
        if required == TaskByIdCapability::WriteAndRead
            && !reader.may_read_tasks(&workspace, agent).await?
        {
            return Ok(false);
        }
        Ok(true)
    }

    async fn workspaces_named(
        &mut self,
        canonical: &CanonicalWorkspaceName,
    ) -> Result<Vec<WorkspaceId>, PersistenceFailure> {
        let mut connection = self.pool.acquire().await.map_err(driver)?;
        TaskWriteAuthorityReader::new(&mut connection)
            .find_workspaces_by_name(canonical)
            .await
    }

    async fn conversation_board(
        &mut self,
        conversation: &ConversationId,
    ) -> Result<Option<BoardKey>, PersistenceFailure> {
        let mut connection = self.pool.acquire().await.map_err(driver)?;
        TaskWriteAuthorityReader::new(&mut connection)
            .conversation_board(conversation)
            .await
    }

    async fn workspace_membership(
        &mut self,
        workspace: &WorkspaceId,
        agent: &AgentId,
    ) -> Result<WorkspaceMembership, PersistenceFailure> {
        let mut connection = self.pool.acquire().await.map_err(driver)?;
        let verdict = TaskWriteAuthorityReader::new(&mut connection)
            .agent_workspace_membership(workspace, agent)
            .await?;
        Ok(match verdict {
            AgentWorkspaceMembership::Absent => WorkspaceMembership::Absent,
            AgentWorkspaceMembership::NotMember => WorkspaceMembership::NotMember,
            AgentWorkspaceMembership::Member => WorkspaceMembership::Member,
        })
    }

    async fn has_capability(
        &mut self,
        capability: TaskWriteCapability,
        workspace: &WorkspaceId,
        agent: &AgentId,
    ) -> Result<bool, PersistenceFailure> {
        let mut connection = self.pool.acquire().await.map_err(driver)?;
        let mut reader = TaskWriteAuthorityReader::new(&mut connection);
        match capability {
            TaskWriteCapability::Create => reader.may_create_tasks(workspace, agent).await,
            TaskWriteCapability::Write => reader.may_write_tasks(workspace, agent).await,
            TaskWriteCapability::Read => reader.may_read_tasks(workspace, agent).await,
        }
    }
}

impl<E> AgentDisplayNameReader for MessagingStore<E>
where
    E: Send + Sync,
{
    async fn agent_display_name(
        &self,
        agent: &AgentId,
    ) -> Result<Option<AgentEventNames>, PersistenceFailure> {
        let row = sqlx::query("SELECT name, display_name FROM agents WHERE id = ?")
            .bind(agent.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(driver)?;
        Ok(row.map(|row| {
            let handle: String = row.get("name");
            let display: Option<String> = row.get("display_name");
            AgentEventNames::new(handle.clone(), display.unwrap_or(handle))
        }))
    }
}

impl<E> silverhand_backend_core::conversation_messaging::messaging::task::AgentExistenceReader
    for MessagingStore<E>
where
    E: Send + Sync,
{
    async fn agent_exists(&self, agent: &AgentId) -> Result<bool, PersistenceFailure> {
        let present: Option<i64> = sqlx::query_scalar("SELECT 1 FROM agents WHERE id = ?")
            .bind(agent.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(driver)?;
        Ok(present.is_some())
    }
}

impl<E> TaskParticipationReads for MessagingStore<E>
where
    E: Send + Sync,
{
    async fn is_agent_participant(
        &mut self,
        conversation: &ConversationId,
        agent: &AgentId,
    ) -> Result<bool, PersistenceFailure> {
        // The point form IS the set read over a set of one, so the two share a single
        // participation predicate ([`AGENT_PARTICIPATION_PREDICATE`]) and can never drift.
        let participating = self
            .agent_participating_conversations(agent, std::slice::from_ref(conversation))
            .await?;
        Ok(participating.contains(conversation))
    }

    async fn agent_participating_conversations(
        &mut self,
        agent: &AgentId,
        conversations: &[ConversationId],
    ) -> Result<HashSet<ConversationId>, PersistenceFailure> {
        // ONE round-trip for the whole candidate set: `c.id IN (...)` narrows the coordinate,
        // the shared participation tail decides admission. An empty input asks nothing. The
        // returned ids are matched back to the input identities (never re-parsed), so the
        // `IN` set is a coordinate narrowing and a conversation the query omits is simply not
        // admitted.
        if conversations.is_empty() {
            return Ok(HashSet::new());
        }
        // The requested set is bound as ONE JSON array (`json_each` drives it); no dynamic
        // SQL. The three trailing binds are the agent for the roster arm and both direct-pair
        // endpoints. The returned ids are matched back to the input identities below, never
        // re-parsed, so the set is a coordinate narrowing.
        let requested = serde_json::to_string(
            &conversations
                .iter()
                .map(ConversationId::as_str)
                .collect::<Vec<_>>(),
        )
        .map_err(|e| PersistenceFailure::from_source(MessagingStoreError::Serialization(e)))?;
        let rows = sqlx::query(SELECT_AGENT_PARTICIPATING_SET)
            .bind(requested)
            .bind(agent.as_str())
            .bind(agent.as_str())
            .bind(agent.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(driver)?;
        let mut admitted: HashSet<&str> = HashSet::new();
        for row in &rows {
            let id: &str = row.try_get("id").map_err(driver)?;
            admitted.insert(id);
        }
        Ok(conversations
            .iter()
            .filter(|conversation| admitted.contains(conversation.as_str()))
            .cloned()
            .collect())
    }

    async fn is_user_participant(
        &mut self,
        conversation: &ConversationId,
        user: &UserId,
    ) -> Result<bool, PersistenceFailure> {
        let hit: Option<i64> = sqlx::query_scalar(SELECT_USER_PARTICIPATES)
            .bind(conversation.as_str())
            .bind(user.as_str())
            .bind(user.as_str())
            .bind(user.as_str())
            .bind(user.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(driver)?;
        Ok(hit.is_some())
    }
}

impl<E> AgentBoardResolver for MessagingStore<E>
where
    E: Send + Sync,
{
    async fn resolve_board(
        &mut self,
        request: &BoardScopeRequest,
    ) -> Result<BoardResolution, PersistenceFailure> {
        let board = match &request.source {
            // A `--workspace-id/--workspace` selector: the agent must be a member of
            // the named workspace, and the board is that workspace's own. The one
            // membership read is three-valued so a non-existent workspace resolves to
            // `NoSuchWorkspace` (→ 404) while an existing workspace the agent is not a
            // member of resolves to `Unauthorized` (→ 403). Neither is corruption.
            BoardScopeSource::SelectedWorkspace(workspace) => {
                let membership: Option<i64> = sqlx::query_scalar(AGENT_WORKSPACE_MEMBERSHIP)
                    .bind(request.agent.as_str())
                    .bind(request.agent.as_str())
                    .bind(workspace.as_str())
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(driver)?;
                match membership {
                    None => return Ok(BoardResolution::NoSuchWorkspace),
                    Some(is_member) if is_member != 0 => BoardKey::Workspace(workspace.clone()),
                    Some(_) => return Ok(BoardResolution::Unauthorized),
                }
            }
            // A conversation frame: the board is the frame's own, derived from its
            // placement. An ABSENT conversation derives no board and denies
            // (`Unauthorized`); the frame's own participation is the downstream gate,
            // not this resolution. But a PRESENT row is a committed v76 fact — its
            // placement is the closed set `workspace|direct_pair` and its shape obliges
            // the matching coordinate — so a missing/unknown/unparseable coordinate on a
            // selected row is schema/projection corruption (safe-500), not a denial.
            BoardScopeSource::FrameConversation(conversation) => {
                let Some(row) = sqlx::query(CONVERSATION_BOARD_PARTS)
                    .bind(conversation.as_str())
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(driver)?
                else {
                    return Ok(BoardResolution::Unauthorized);
                };
                let placement: String = row.try_get("placement_kind").map_err(driver)?;
                match placement.as_str() {
                    PLACEMENT_WORKSPACE => {
                        let workspace: Option<String> =
                            row.try_get("workspace_id").map_err(driver)?;
                        let workspace =
                            workspace.ok_or_else(|| corrupt("conversations.workspace_id"))?;
                        BoardKey::Workspace(
                            WorkspaceId::parse(workspace)
                                .map_err(|_| corrupt("conversations.workspace_id"))?,
                        )
                    }
                    PLACEMENT_DIRECT_PAIR => {
                        let pair: Option<String> = row.try_get("pair_key").map_err(driver)?;
                        let pair = pair.ok_or_else(|| corrupt("conversations.pair_key"))?;
                        BoardKey::Pair(
                            PairKey::parse(pair).map_err(|_| corrupt("conversations.pair_key"))?,
                        )
                    }
                    _ => return Err(corrupt("conversations.placement_kind")),
                }
            }
        };
        // Echo the subject the request named so `authorize_agent_board` can bind
        // the answer to the request; a mis-routed answer mints no witness.
        Ok(BoardResolution::Resolved(ResolvedBoard {
            agent: request.agent.clone(),
            source: request.source.clone(),
            board,
        }))
    }
}

impl<E> AgentReadableBoards for MessagingStore<E>
where
    E: Send + Sync,
{
    async fn readable_boards(
        &mut self,
        agent: &AgentId,
    ) -> Result<Vec<ReadableBoard>, PersistenceFailure> {
        let mut boards = Vec::new();

        // Workspace boards: member/owner union. `read_tasks` is folded downstream, so a
        // membership hit alone enters the enumeration (the minter drops a board the agent
        // may belong to but cannot read tasks on).
        let workspace_rows = sqlx::query(AGENT_READABLE_WORKSPACES)
            .bind(agent.as_str())
            .bind(agent.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(driver)?;
        for row in &workspace_rows {
            let id: String = row.try_get("id").map_err(driver)?;
            let workspace = WorkspaceId::parse(id).map_err(|_| corrupt("workspaces.id"))?;
            boards.push(ReadableBoard {
                agent: agent.clone(),
                board: BoardKey::Workspace(workspace),
            });
        }

        // Direct-pair boards the agent is an endpoint of. A selected direct-pair row is a
        // committed fact obliged to carry its `pair_key`, so a missing/unparseable one is
        // corruption (safe-500), never a silently-dropped board.
        let pair_rows = sqlx::query(AGENT_DIRECT_PAIR_BOARDS)
            .bind(PLACEMENT_DIRECT_PAIR)
            .bind(ENDPOINT_KIND_AGENT)
            .bind(agent.as_str())
            .bind(ENDPOINT_KIND_AGENT)
            .bind(agent.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(driver)?;
        for row in &pair_rows {
            let pair: Option<String> = row.try_get("pair_key").map_err(driver)?;
            let pair = pair.ok_or_else(|| corrupt("conversations.pair_key"))?;
            let pair = PairKey::parse(pair).map_err(|_| corrupt("conversations.pair_key"))?;
            boards.push(ReadableBoard {
                agent: agent.clone(),
                board: BoardKey::Pair(pair),
            });
        }

        Ok(boards)
    }
}

// -- The internal peer-profile route's caller-side gate ports (#2226) ---------
//
// These three ports back the caller-side ladder of `admit_peer_profile`: the
// conversation floor (placement), the cross-workspace horizon, and the always-on
// `read_messages` capability. They mint NO new authority — each reuses a spelling
// this store already owns (the placement decoder, the membership predicate, the
// capability shape) — so the peer route reads through the SAME truth the task board
// and send gate do. The trait declarations live in the #2226 consumer module; only
// the production spelling lives here.

impl<E> CallerConversationPlacement for MessagingStore<E>
where
    E: Send + Sync,
{
    async fn conversation_placement(
        &self,
        conversation: &ConversationId,
    ) -> Result<Option<WorkspacePlacement>, PersistenceFailure> {
        // Reuse the ONE placement decoder. `authority_conversation` runs `decode_placement`,
        // which already guarantees `workspace.is_some() ⟺ kind == workspace` and turns an
        // illegal placement or a missing coordinate on a selected row into corruption
        // (safe-500, propagated by `?`). So mapping on `.workspace` is total: `Some` is a
        // real workspace board, `None` is a direct pair (workspace-less); a missing
        // conversation is honestly `None` at the outer `Option` (→ the route's 404).
        Ok(self
            .authority_conversation(conversation)
            .await?
            .map(|placement| match placement.workspace {
                Some(workspace) => WorkspacePlacement::Workspace(workspace),
                None => WorkspacePlacement::WorkspaceLess,
            }))
    }
}

impl<E> CallerWorkspaceHorizon for MessagingStore<E>
where
    E: Send + Sync,
{
    async fn can_access_workspace(
        &self,
        agent: &AgentId,
        workspace: &WorkspaceId,
    ) -> Result<bool, PersistenceFailure> {
        // Reuse the horizon predicate: members ∪ owned agent-home, exactly the axis
        // [`AGENT_WORKSPACE_MEMBERSHIP`] answers for the task board's 404-vs-403 split. Here
        // it is folded to the boolean the caller gate needs — no row / `0` → outside the
        // horizon, `1` → reachable — because target existence is settled by the read that
        // follows, not by this gate.
        let reachable: Option<i64> = sqlx::query_scalar(AGENT_WORKSPACE_MEMBERSHIP)
            .bind(agent.as_str())
            .bind(agent.as_str())
            .bind(workspace.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(driver)?;
        Ok(reachable.is_some_and(|is_member| is_member != 0))
    }
}

impl<E> CallerReadMessagesCapability for MessagingStore<E>
where
    E: Send + Sync,
{
    async fn may_read_messages(
        &self,
        agent: &AgentId,
        workspace: &WorkspaceId,
    ) -> Result<bool, PersistenceFailure> {
        crate::foundation::capability::agent_has_workspace_capability(
            &self.pool,
            workspace.as_str(),
            agent.as_str(),
            "read_messages",
        )
        .await
        .map_err(driver)
    }
}

impl<E> silverhand_backend_core::conversation_messaging::messaging::collaboration::channel_fork::CreateChannelsCapability
    for MessagingStore<E>
where
    E: Send + Sync,
{
    async fn may_create_channels(
        &self,
        agent: &AgentId,
        workspace: &WorkspaceId,
    ) -> Result<bool, PersistenceFailure> {
        crate::foundation::capability::agent_has_workspace_capability(&self.pool, workspace.as_str(), agent.as_str(), "create_channels")
            .await
            .map_err(driver)
    }
}

impl<E: Send + Sync> MessagingStore<E> {
    async fn visible_peer_dm_scope_refusal(
        &self,
        caller: &AgentId,
        peer: &AgentId,
    ) -> Result<bool, PersistenceFailure> {
        let mut tx = self
            .pool
            .begin_with("BEGIN DEFERRED")
            .await
            .map_err(driver)?;
        let blocked: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM agents WHERE (id=? OR id=?) AND session_scope='conversation')")
            .bind(caller.as_str()).bind(peer.as_str()).fetch_one(&mut *tx).await.map_err(driver)?;
        if blocked == 0 {
            return Ok(false);
        }
        let workspaces: Vec<String> =
            sqlx::query_scalar("SELECT workspace_id FROM workspace_members WHERE agent_id=?")
                .bind(peer.as_str())
                .fetch_all(&mut *tx)
                .await
                .map_err(driver)?;
        for workspace in workspaces {
            let reachable: Option<i64> = sqlx::query_scalar(AGENT_WORKSPACE_MEMBERSHIP)
                .bind(caller.as_str())
                .bind(caller.as_str())
                .bind(&workspace)
                .fetch_optional(&mut *tx)
                .await
                .map_err(driver)?;
            if reachable != Some(1) {
                continue;
            }
            let readable = crate::foundation::capability::agent_has_workspace_capability(
                &mut *tx,
                &workspace,
                caller.as_str(),
                "read_messages",
            )
            .await
            .map_err(driver)?;
            if readable {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::{ParsedTarget, parse_target};

    #[test]
    fn parser_keeps_the_three_published_arms_disjoint() {
        assert!(matches!(
            parse_target("#conv:aBcDeFgHiJkLmN_P"),
            Some(ParsedTarget::Conversation { thread: None, .. })
        ));
        for malformed in [
            "#silverhand/%20general",
            "#silverhand/general%20",
            "#silverhand/gene\u{200d}ral",
            "#silverhand/❤\u{fe0f}",
            "#silverhand/1\u{20e3}",
            "#silverhand/\u{1f1e8}\u{1f1f3}",
        ] {
            assert!(parse_target(malformed).is_none(), "{malformed:?}");
        }
        assert!(matches!(
            parse_target("#conv:aBcDeFgHiJkLmN_P/room"),
            Some(ParsedTarget::Named { thread: None, .. })
        ));
        assert!(matches!(
            parse_target("#silverhand/thread:aBcDeFgHiJkLmN_P"),
            Some(ParsedTarget::Named { thread: None, .. })
        ));
        assert!(matches!(
            parse_target("#silverhand/thread:aBcDeFgHiJkLmN_P/thread:root000000000001"),
            Some(ParsedTarget::Named {
                thread: Some(_),
                ..
            })
        ));
    }
}
