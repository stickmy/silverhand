//! SQLite adapter for the narrow internal agent-profile read.
//!
//! It supplies the facts [`silverhand_backend_core::identity_access::agent_profile_read`]
//! declares — one agent's `{ id, name, display_name, description, avatar }` —
//! and NOTHING wider. Unlike [`crate::identity_access::agent_detail_read`] it takes no host
//! `LEFT JOIN` and runs no `agent_view_acl`: the internal profile routes are
//! self-pinned or peer-gated in the route's own gate, never judged against a
//! viewer's arms, so this read carries no visibility authority to leak into
//! them.
//!
//! ## The id is re-sourced, not trusted from the query
//!
//! The query binds a `:aid`, but the fact's [`AgentId`] is decoded from the
//! RETURNED row's `id` column through the domain's fallible parser. A route
//! binds its response to that sealed id rather than to the coordinate it asked
//! for, so a peer read (which will look up by handle) hands back the target's
//! real id and a corrupt empty id fails closed as a read error, never a silent
//! coercion.
//!
//! ## The avatar folds to a revision, never a storage locator
//!
//! `agents.avatar_storage_key` + `updated_at` collapse HERE into
//! `Option<AvatarRevision>` using the same truthiness rule the canonical
//! `avatar_url` derivation applies (a non-empty key is an avatar; empty/NULL is
//! none). The storage key never crosses this boundary — the profile face needs
//! only presence and the cache-buster, and only a route that actually streams
//! the blob consumes a typed blob ref.

use std::{path::Path, time::Duration};

use silverhand_backend_core::{
    conversation_messaging::messaging::AvatarRevision,
    foundation::identity::AgentId,
    foundation::persistence::PersistenceFailure,
    identity_access::agent_profile_read::{AgentProfileFact, AgentProfileReadModel},
};
use sqlx::{Row, SqlitePool, sqlite::SqliteRow};
use thiserror::Error;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The narrow profile read: only the columns the internal profile DTO and its
/// avatar-URL derivation consume. No host join, no ACL SQL, no wire-vocabulary
/// columns (runtime/model/…) — those belong to the detail read, not here. The
/// two lookups (by id, by handle) share this projection and differ only in the
/// predicate they append, so both decode one row shape through
/// [`decode_profile_row`]. Composed as `&'static str` because sqlx accepts no
/// runtime-built statement.
macro_rules! select_agent_profile_from {
    () => {
        "SELECT id, name, display_name, description, avatar_storage_key, updated_at FROM agents"
    };
}

/// By id — the self profile read.
const SELECT_AGENT_PROFILE_BY_ID: &str = concat!(select_agent_profile_from!(), " WHERE id = ?");

/// By handle — the peer read. `agents.name` is matched EXACTLY (no `COLLATE`), so
/// a different case is a different handle, mirroring the product's `getByName`.
const SELECT_AGENT_PROFILE_BY_HANDLE: &str =
    concat!(select_agent_profile_from!(), " WHERE name = ?");

#[derive(Clone)]
pub struct SqliteAgentProfileRead {
    pool: SqlitePool,
}

/// Read failures surfaced only as the source of a [`PersistenceFailure`]. Every
/// variant is a driver fault or corruption of a committed row, never a
/// product-level refusal — an absent row is `Ok(None)`, the route's `404`.
#[derive(Debug, Error)]
pub enum AgentProfileReadError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored agents.id does not name a usable identity")]
    MalformedIdentity,
}

/// Decode one profile row: the id is re-sourced through the domain's fallible
/// parser (an empty stored id is corruption, a safe `500`, never coerced), and
/// the avatar folds by TRUTHINESS — a non-empty `avatar_storage_key` + the row's
/// `updated_at` collapse to `Some(AvatarRevision)`, the same rule the canonical
/// `avatar_url` applies; the storage key itself never leaves this decode.
fn decode_profile_row(row: &SqliteRow) -> Result<AgentProfileFact, AgentProfileReadError> {
    let stored_id: String = row.try_get("id")?;
    let id = AgentId::parse(stored_id).map_err(|_| AgentProfileReadError::MalformedIdentity)?;

    let avatar_storage_key: Option<String> = row.try_get("avatar_storage_key")?;
    let updated_at: i64 = row.try_get("updated_at")?;
    let avatar = avatar_storage_key
        .filter(|key| !key.is_empty())
        .map(|_| AvatarRevision::at(updated_at));

    Ok(AgentProfileFact {
        id,
        name: row.try_get("name")?,
        display_name: row.try_get("display_name")?,
        description: row.try_get("description")?,
        avatar,
    })
}

impl SqliteAgentProfileRead {
    pub fn from_pool(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn open(path: impl AsRef<Path>) -> Result<Self, AgentProfileReadError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self { pool })
    }

    /// Read one profile row for `statement` bound to `key`. Shared by the id and
    /// handle lookups so both use one statement's snapshot and decode
    /// identically; only the predicate the statement carries differs.
    async fn read_profile_where(
        &self,
        statement: &'static str,
        key: &str,
    ) -> Result<Option<AgentProfileFact>, AgentProfileReadError> {
        let row = sqlx::query(statement)
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        let fact = match row {
            Some(row) => Some(decode_profile_row(&row)?),
            None => None,
        };

        Ok(fact)
    }
}

impl AgentProfileReadModel for SqliteAgentProfileRead {
    async fn read_agent_profile(
        &self,
        agent: &AgentId,
    ) -> Result<Option<AgentProfileFact>, PersistenceFailure> {
        self.read_profile_where(SELECT_AGENT_PROFILE_BY_ID, agent.as_str())
            .await
            .map_err(PersistenceFailure::from_source)
    }

    async fn read_agent_profile_by_handle(
        &self,
        handle: &str,
    ) -> Result<Option<AgentProfileFact>, PersistenceFailure> {
        self.read_profile_where(SELECT_AGENT_PROFILE_BY_HANDLE, handle)
            .await
            .map_err(PersistenceFailure::from_source)
    }
}
