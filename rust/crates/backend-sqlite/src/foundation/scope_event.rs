//! SQLite authorization reads for the authenticated product-event transport.

use std::{error::Error, path::Path, time::Duration};

use silverhand_backend_core::{
    foundation::identity::{ConversationId, UserId},
    foundation::persistence::PersistenceFailure,
    foundation::scope_event::{AuthorizeEventScope, EventScope},
};
use sqlx::{Row, SqlitePool};
use thiserror::Error;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const SELECT_WORKSPACE_MEMBER: &str = "SELECT EXISTS(SELECT 1 FROM workspaces w JOIN workspace_users wu ON wu.workspace_id = w.id WHERE w.id = ? AND wu.user_id = ?)";
const SELECT_CONVERSATION: &str = "SELECT placement_kind, roster_policy, visibility, archived_at, workspace_id, endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id FROM conversations WHERE id = ?";
const SELECT_WORKSPACE_CONVERSATION_MEMBERSHIP: &str = "SELECT EXISTS(SELECT 1 FROM conversation_users cu JOIN workspace_users wu ON wu.workspace_id = ? AND wu.user_id = cu.user_id WHERE cu.conversation_id = ? AND cu.user_id = ?)";

#[derive(Clone)]
pub struct SqliteEventScopeAuthority {
    pool: SqlitePool,
}

#[derive(Debug, Error)]
pub enum SqliteEventScopeAuthorityError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
}

impl SqliteEventScopeAuthority {
    pub fn from_pool(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn open(path: impl AsRef<Path>) -> Result<Self, SqliteEventScopeAuthorityError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self { pool })
    }

    async fn workspace_member(
        &self,
        workspace: &str,
        viewer: &UserId,
    ) -> Result<bool, PersistenceFailure> {
        let exists: i64 = sqlx::query_scalar(SELECT_WORKSPACE_MEMBER)
            .bind(workspace)
            .bind(viewer.as_str())
            .fetch_one(&self.pool)
            .await
            .map_err(persistence)?;
        Ok(exists != 0)
    }

    async fn conversation_access(
        &self,
        conversation: &ConversationId,
        viewer: &UserId,
        allow_public: bool,
    ) -> Result<bool, PersistenceFailure> {
        let row = sqlx::query(SELECT_CONVERSATION)
            .bind(conversation.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(persistence)?;
        let Some(row) = row else { return Ok(false) };

        let placement: String = row.try_get("placement_kind").map_err(persistence)?;
        let roster: String = row.try_get("roster_policy").map_err(persistence)?;
        let visibility: String = row.try_get("visibility").map_err(persistence)?;
        let workspace_raw: Option<String> = row.try_get("workspace_id").map_err(persistence)?;
        let a_kind: Option<String> = row.try_get("endpoint_a_kind").map_err(persistence)?;
        let a_id: Option<String> = row.try_get("endpoint_a_id").map_err(persistence)?;
        let b_kind: Option<String> = row.try_get("endpoint_b_kind").map_err(persistence)?;
        let b_id: Option<String> = row.try_get("endpoint_b_id").map_err(persistence)?;
        match placement.as_str() {
            "workspace" => {
                if !matches!(roster.as_str(), "open" | "fixed")
                    || !matches!(visibility.as_str(), "private" | "public")
                    || a_kind.is_some()
                    || a_id.is_some()
                    || b_kind.is_some()
                    || b_id.is_some()
                {
                    return Err(PersistenceFailure::from_source(InvalidPlacement));
                }
                let workspace = workspace_raw
                    .ok_or_else(|| PersistenceFailure::from_source(MissingPersistedCoordinate))?;
                let workspace =
                    silverhand_backend_core::foundation::identity::WorkspaceId::parse(workspace)
                        .map_err(|source| {
                            PersistenceFailure::from_source(InvalidPersistedScope(source))
                        })?;
                let member: i64 = sqlx::query_scalar(SELECT_WORKSPACE_CONVERSATION_MEMBERSHIP)
                    .bind(workspace.as_str())
                    .bind(conversation.as_str())
                    .bind(viewer.as_str())
                    .fetch_one(&self.pool)
                    .await
                    .map_err(persistence)?;
                if member != 0 {
                    return Ok(true);
                }
                if !allow_public {
                    return Ok(false);
                }
                let archived_at: Option<i64> = row.try_get("archived_at").map_err(persistence)?;
                if roster != "open" || visibility != "public" || archived_at.is_some() {
                    return Ok(false);
                }
                self.workspace_member(workspace.as_str(), viewer).await
            }
            "direct_pair" => {
                if workspace_raw.is_some() || roster != "fixed" || visibility != "private" {
                    return Err(PersistenceFailure::from_source(InvalidPlacement));
                }
                let a_kind = a_kind
                    .ok_or_else(|| PersistenceFailure::from_source(MissingPersistedCoordinate))?;
                let a_id = a_id
                    .ok_or_else(|| PersistenceFailure::from_source(MissingPersistedCoordinate))?;
                let b_kind = b_kind
                    .ok_or_else(|| PersistenceFailure::from_source(MissingPersistedCoordinate))?;
                let b_id = b_id
                    .ok_or_else(|| PersistenceFailure::from_source(MissingPersistedCoordinate))?;
                let a = decode_endpoint(&a_kind, a_id)?;
                let b = decode_endpoint(&b_kind, b_id)?;
                Ok(matches!(a, Endpoint::Human(ref id) if id == viewer)
                    || matches!(b, Endpoint::Human(ref id) if id == viewer))
            }
            _ => Err(PersistenceFailure::from_source(InvalidPlacement)),
        }
    }
}

impl AuthorizeEventScope for SqliteEventScopeAuthority {
    async fn authorize_event_scope(
        &self,
        viewer: &UserId,
        scope: &EventScope,
    ) -> Result<bool, PersistenceFailure> {
        match scope {
            EventScope::App => Ok(true),
            EventScope::Agent(agent) => {
                let mut tx = self.pool.begin().await.map_err(persistence)?;
                let allowed = crate::identity_access::agent_view_acl::may_read_agent_stream(
                    &mut tx, viewer, agent,
                )
                .await
                .map_err(persistence)?;
                tx.commit().await.map_err(persistence)?;
                Ok(allowed)
            }
            EventScope::Viewer(owner) => Ok(owner == viewer),
            EventScope::Workspace(workspace) => {
                self.workspace_member(workspace.as_str(), viewer).await
            }
            EventScope::Conversation(conversation) => {
                self.conversation_access(conversation, viewer, true).await
            }
            EventScope::ConversationSide(conversation) => {
                self.conversation_access(conversation, viewer, false).await
            }
        }
    }
}

enum Endpoint {
    Human(UserId),
    Agent,
}

fn decode_endpoint(kind: &str, id: String) -> Result<Endpoint, PersistenceFailure> {
    match kind {
        "human" => UserId::parse(id)
            .map(Endpoint::Human)
            .map_err(|source| PersistenceFailure::from_source(InvalidPersistedScope(source))),
        "agent" => silverhand_backend_core::foundation::identity::AgentId::parse(id)
            .map(|_| Endpoint::Agent)
            .map_err(|source| PersistenceFailure::from_source(InvalidPersistedScope(source))),
        _ => Err(PersistenceFailure::from_source(InvalidEndpointKind)),
    }
}

fn persistence(error: sqlx::Error) -> PersistenceFailure {
    PersistenceFailure::from_source(error)
}

#[derive(Debug, Error)]
#[error("persisted event-scope coordinate is invalid")]
struct InvalidPersistedScope<E: Error + Send + Sync + 'static>(#[source] E);

#[derive(Debug, Error)]
#[error("persisted conversation placement is invalid")]
struct InvalidPlacement;

#[derive(Debug, Error)]
#[error("persisted conversation endpoint kind is invalid")]
struct InvalidEndpointKind;

#[derive(Debug, Error)]
#[error("persisted conversation is missing a required coordinate")]
struct MissingPersistedCoordinate;
