//! SQLite cold-read adapter for the authenticated viewer's complete Direct Messages list.

use silverhand_backend_observability::Observe;
use std::{collections::HashMap, path::Path, time::Duration};

use silverhand_backend_core::{
    conversation_messaging::direct_pair_read::{
        DirectPairCounterpart, DirectPairList, DirectPairListItem, DirectPairReadModel,
        DirectPairUnreadCount,
    },
    conversation_messaging::messaging::{AvatarRevision, PrincipalIdentity},
    foundation::identity::{AgentId, ConversationId, UserId},
    foundation::persistence::PersistenceFailure,
    identity_access::agents::runtime::RuntimeKind,
};
use sqlx::{Row, Sqlite, Transaction};
use thiserror::Error;

use crate::viewer_read_model::unread_read::{UnreadReadModelError, conversation_unread_counts};

const DEFERRED_READ_BEGIN: &str = "BEGIN DEFERRED";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const SELECT_PAIRS: &str = "SELECT c.id, c.endpoint_a_kind, c.endpoint_a_id, c.endpoint_a_deleted_at, \
     c.endpoint_b_kind, c.endpoint_b_id, c.endpoint_b_deleted_at, c.created_at \
     FROM conversations c WHERE c.placement_kind = 'direct_pair' AND \
     ((c.endpoint_a_kind = 'human' AND c.endpoint_a_id = ?) OR \
      (c.endpoint_b_kind = 'human' AND c.endpoint_b_id = ?)) \
     ORDER BY c.created_at ASC, c.id ASC";

const SELECT_AGENTS: &str = "SELECT a.id, a.name, a.display_name, a.avatar_storage_key, a.updated_at, a.runtime, a.session_scope \
     FROM json_each(?) requested JOIN agents a ON a.id = requested.value";
const SELECT_USERS: &str = "SELECT u.id, u.name, u.display_name, u.avatar_storage_key, u.updated_at \
     FROM json_each(?) requested JOIN users u ON u.id = requested.value";

#[derive(Clone)]
pub struct SqliteDirectPairReadModel {
    pool: crate::foundation::database::SqliteDatabase,
}

#[derive(Debug, Error)]
pub enum DirectPairReadError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("read-model coordinate serialization failed")]
    Serialization(#[from] serde_json::Error),
    #[error("unread roll-up failed")]
    Unread(#[source] UnreadReadModelError),
    #[error("stored column {field} does not name a usable identity")]
    MalformedIdentity { field: &'static str },
    #[error("stored endpoint kind is outside the closed human/agent set")]
    UnknownEndpointKind,
    #[error("stored runtime is outside the closed runtime set")]
    UnknownRuntime,
    #[error("unread roll-up omitted a requested direct pair")]
    MissingUnreadCount,
    #[error("stored unread roll-up is negative")]
    NegativeUnreadCount,
}

impl SqliteDirectPairReadModel {
    pub fn from_pool(pool: impl Into<crate::foundation::database::SqliteDatabase>) -> Self {
        let pool = pool.into();
        Self { pool }
    }

    pub async fn open(path: impl AsRef<Path>) -> Result<Self, DirectPairReadError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self { pool })
    }

    #[tracing::instrument(
        target = "silverhand::diagnostics",
        name = "db.direct_pairs",
        skip_all,
        fields(
            elapsed_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
            operation_finished = tracing::field::Empty,
        )
    )]
    async fn list_transaction(
        &self,
        viewer: &UserId,
    ) -> Result<DirectPairList, DirectPairReadError> {
        let observation = silverhand_backend_observability::Operation::current();
        let result = async {
            let mut tx =
                crate::foundation::statement_metrics::begin(&self.pool, Some(DEFERRED_READ_BEGIN))
                    .await?;
            let rows = sqlx::query(SELECT_PAIRS)
                .bind(viewer.as_str())
                .bind(viewer.as_str())
                .fetch_all(&mut *tx)
                .observe(silverhand_backend_observability::operation_span!(
                    "db.direct_pairs.rows"
                ))
                .await?;
            let mut evidence = Vec::with_capacity(rows.len());
            for row in rows {
                evidence.push(PairEvidence::decode(&row)?);
            }

            let principals = LivePrincipals::load(&mut tx, &evidence).await?;
            let conversations = evidence
                .iter()
                .map(|pair| pair.conversation.clone())
                .collect::<Vec<_>>();
            let mut unread = conversation_unread_counts(&mut tx, viewer, &conversations)
                .await
                .map_err(DirectPairReadError::Unread)?;

            let mut items = Vec::with_capacity(evidence.len());
            for pair in evidence {
                let counterpart = pair.counterpart(viewer)?;
                let counterpart_value = principals.counterpart(&counterpart)?;
                let read_only = pair.endpoints().iter().any(|endpoint| {
                    endpoint.deleted_at_ms.is_some() || !principals.allows_direct_messages(endpoint)
                });
                let unread_count = unread
                    .remove(&pair.conversation)
                    .ok_or(DirectPairReadError::MissingUnreadCount)?;
                let unread_count = DirectPairUnreadCount::try_from(unread_count)
                    .map_err(|_| DirectPairReadError::NegativeUnreadCount)?;
                items.push(DirectPairListItem::projected(
                    pair.conversation,
                    counterpart_value,
                    unread_count,
                    read_only,
                    pair.created_at_ms,
                ));
            }
            tx.commit().await?;
            Ok(DirectPairList::complete(items))
        }
        .await;
        observation.finish(&result);
        result
    }
}

impl DirectPairReadModel for SqliteDirectPairReadModel {
    async fn list_direct_pairs(
        &self,
        viewer: &UserId,
    ) -> Result<DirectPairList, PersistenceFailure> {
        self.list_transaction(viewer)
            .await
            .map_err(PersistenceFailure::from_source)
    }
}

#[derive(Clone)]
pub(crate) struct PairEvidence {
    conversation: ConversationId,
    a: Endpoint,
    b: Endpoint,
    created_at_ms: i64,
}

impl PairEvidence {
    pub(crate) fn decode(row: &sqlx::sqlite::SqliteRow) -> Result<Self, DirectPairReadError> {
        Ok(Self {
            conversation: conversation_id(row.try_get("id")?, "conversations.id")?,
            a: Endpoint::decode(
                row.try_get("endpoint_a_kind")?,
                row.try_get("endpoint_a_id")?,
                row.try_get("endpoint_a_deleted_at")?,
                "conversations.endpoint_a_id",
            )?,
            b: Endpoint::decode(
                row.try_get("endpoint_b_kind")?,
                row.try_get("endpoint_b_id")?,
                row.try_get("endpoint_b_deleted_at")?,
                "conversations.endpoint_b_id",
            )?,
            created_at_ms: row.try_get("created_at")?,
        })
    }

    fn endpoints(&self) -> [&Endpoint; 2] {
        [&self.a, &self.b]
    }

    /// The conversation this pair names. `pub(crate)`: the agent-audit chats list
    /// reuses this pair decode + counterpart resolution for its direct-pair arm.
    pub(crate) fn conversation(&self) -> &ConversationId {
        &self.conversation
    }

    /// The pair's creation instant in ms — the audit list's `created_at` and part of
    /// its sort key.
    pub(crate) fn created_at_ms(&self) -> i64 {
        self.created_at_ms
    }

    /// Both endpoints, so the audit arm can compute the survivor read-only fact the
    /// same way the DM list does.
    pub(crate) fn both_endpoints(&self) -> [&Endpoint; 2] {
        self.endpoints()
    }

    /// The endpoint that is NOT the audited agent. Exactly one endpoint must name the
    /// agent; a pair where both or neither do is a decode defect — never a guessed
    /// counterpart — so it fails closed, mirroring the TS `resolveCounterpart` match
    /// count.
    pub(crate) fn counterpart_of_agent(
        &self,
        agent: &AgentId,
    ) -> Result<&Endpoint, DirectPairReadError> {
        let a_is_agent = matches!(&self.a.identity, PrincipalIdentity::Agent(id) if id == agent);
        let b_is_agent = matches!(&self.b.identity, PrincipalIdentity::Agent(id) if id == agent);
        match (a_is_agent, b_is_agent) {
            (true, false) => Ok(&self.b),
            (false, true) => Ok(&self.a),
            _ => Err(DirectPairReadError::MalformedIdentity {
                field: "conversations.direct_pair_endpoints",
            }),
        }
    }

    pub(crate) fn counterpart(&self, viewer: &UserId) -> Result<Endpoint, DirectPairReadError> {
        let a_is_viewer = matches!(&self.a.identity, PrincipalIdentity::Human(id) if id == viewer);
        let b_is_viewer = matches!(&self.b.identity, PrincipalIdentity::Human(id) if id == viewer);
        match (a_is_viewer, b_is_viewer) {
            (true, false) => Ok(self.b.clone()),
            (false, true) => Ok(self.a.clone()),
            _ => Err(DirectPairReadError::MalformedIdentity {
                field: "conversations.direct_pair_endpoints",
            }),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Endpoint {
    identity: PrincipalIdentity,
    deleted_at_ms: Option<i64>,
}

impl Endpoint {
    fn decode(
        kind: String,
        id: String,
        deleted_at_ms: Option<i64>,
        field: &'static str,
    ) -> Result<Self, DirectPairReadError> {
        let identity = match kind.as_str() {
            "human" => PrincipalIdentity::Human(
                UserId::parse(id).map_err(|_| DirectPairReadError::MalformedIdentity { field })?,
            ),
            "agent" => PrincipalIdentity::Agent(
                AgentId::parse(id).map_err(|_| DirectPairReadError::MalformedIdentity { field })?,
            ),
            _ => return Err(DirectPairReadError::UnknownEndpointKind),
        };
        Ok(Self {
            identity,
            deleted_at_ms,
        })
    }

    /// Whether this endpoint carries a deletion tombstone. `pub(crate)`: the audit
    /// direct-pair arm folds this into the survivor read-only fact.
    pub(crate) fn is_deleted(&self) -> bool {
        self.deleted_at_ms.is_some()
    }
}

#[derive(Default)]
pub(crate) struct LivePrincipals {
    agents: HashMap<String, LivePrincipal>,
    users: HashMap<String, LivePrincipal>,
}

#[derive(Clone)]
struct LivePrincipal {
    name: String,
    display_name: String,
    avatar: Option<AvatarRevision>,
    runtime: Option<RuntimeKind>,
    allows_direct_messages: bool,
}

impl LivePrincipals {
    pub(crate) async fn load(
        tx: &mut Transaction<'_, Sqlite>,
        pairs: &[PairEvidence],
    ) -> Result<Self, DirectPairReadError> {
        let mut agent_ids = Vec::new();
        let mut user_ids = Vec::new();
        for endpoint in pairs.iter().flat_map(PairEvidence::endpoints) {
            match &endpoint.identity {
                PrincipalIdentity::Agent(id) => agent_ids.push(id.as_str()),
                PrincipalIdentity::Human(id) => user_ids.push(id.as_str()),
            }
        }
        agent_ids.sort_unstable();
        agent_ids.dedup();
        user_ids.sort_unstable();
        user_ids.dedup();

        let mut live = Self::default();
        if !agent_ids.is_empty() {
            let requested = serde_json::to_string(&agent_ids)?;
            for row in sqlx::query(SELECT_AGENTS)
                .bind(requested)
                .fetch_all(&mut **tx)
                .observe(silverhand_backend_observability::operation_span!(
                    "db.principals.agents"
                ))
                .await?
            {
                let id: String = row.try_get("id")?;
                let runtime: String = row.try_get("runtime")?;
                let runtime =
                    RuntimeKind::parse(&runtime).ok_or(DirectPairReadError::UnknownRuntime)?;
                live.agents.insert(id, live_principal(&row, Some(runtime))?);
            }
        }
        if !user_ids.is_empty() {
            let requested = serde_json::to_string(&user_ids)?;
            for row in sqlx::query(SELECT_USERS)
                .bind(requested)
                .fetch_all(&mut **tx)
                .observe(silverhand_backend_observability::operation_span!(
                    "db.principals.users"
                ))
                .await?
            {
                let id: String = row.try_get("id")?;
                live.users.insert(id, live_principal(&row, None)?);
            }
        }
        Ok(live)
    }

    /// Whether this live endpoint supports DM writes. The audit projection shares
    /// this rule; conversation-scoped agents remain visible but read-only.
    pub(crate) fn allows_direct_messages(&self, endpoint: &Endpoint) -> bool {
        match &endpoint.identity {
            PrincipalIdentity::Agent(id) => self
                .agents
                .get(id.as_str())
                .is_some_and(|p| p.allows_direct_messages),
            PrincipalIdentity::Human(id) => self.users.contains_key(id.as_str()),
        }
    }

    pub(crate) fn counterpart(
        &self,
        endpoint: &Endpoint,
    ) -> Result<DirectPairCounterpart, DirectPairReadError> {
        if endpoint.deleted_at_ms.is_some() {
            return Ok(DirectPairCounterpart::deleted(endpoint.identity.clone()));
        }
        let principal = match &endpoint.identity {
            PrincipalIdentity::Agent(id) => self.agents.get(id.as_str()),
            PrincipalIdentity::Human(id) => self.users.get(id.as_str()),
        };
        let Some(principal) = principal else {
            return Ok(DirectPairCounterpart::deleted(endpoint.identity.clone()));
        };
        match &endpoint.identity {
            PrincipalIdentity::Agent(id) => Ok(DirectPairCounterpart::active_agent(
                id.clone(),
                principal.name.clone(),
                principal.display_name.clone(),
                principal.avatar,
                principal
                    .runtime
                    .ok_or(DirectPairReadError::UnknownRuntime)?,
            )),
            PrincipalIdentity::Human(id) => Ok(DirectPairCounterpart::active_human(
                id.clone(),
                principal.name.clone(),
                principal.display_name.clone(),
                principal.avatar,
            )),
        }
    }
}

fn live_principal(
    row: &sqlx::sqlite::SqliteRow,
    runtime: Option<RuntimeKind>,
) -> Result<LivePrincipal, DirectPairReadError> {
    let avatar_storage_key: Option<String> = row.try_get("avatar_storage_key")?;
    let updated_at: i64 = row.try_get("updated_at")?;
    Ok(LivePrincipal {
        name: row.try_get("name")?,
        display_name: row.try_get("display_name")?,
        avatar: avatar_storage_key
            .as_deref()
            .filter(|key| !key.is_empty())
            .map(|_| AvatarRevision::at(updated_at)),
        runtime,
        allows_direct_messages: runtime.is_none()
            || row.try_get::<String, _>("session_scope")? == "agent",
    })
}

fn conversation_id(
    raw: String,
    field: &'static str,
) -> Result<ConversationId, DirectPairReadError> {
    ConversationId::parse(raw).map_err(|_| DirectPairReadError::MalformedIdentity { field })
}
