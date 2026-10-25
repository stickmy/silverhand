//! SQLite adapter for the regular-workspace name namespace behind
//! `GET /api/workspaces/name-availability`.
//!
//! The namespace is REGULAR workspaces only: an agent-home workspace stores a
//! `NULL` `name_key` (`owner_agent_id IS NOT NULL`), and the schema's
//! `CHECK ((owner_agent_id IS NULL) = (name_key IS NOT NULL))` guarantees the two
//! are never both set. The lookup therefore scopes to `owner_agent_id IS NULL`
//! and compares the pinned fold key the domain already produced — the SAME key
//! the create path stores and the partial unique index
//! `idx_workspaces_name_key ... WHERE owner_agent_id IS NULL` enforces. The
//! adapter returns the row's OWN stored key rather than a boolean, so the
//! driver-free [`probe_workspace_name`](silverhand_backend_core::identity_access::workspace_name_availability::probe_workspace_name)
//! can bind a positive answer against the key it asked about.

use std::path::Path;

use silverhand_backend_core::{
    foundation::persistence::PersistenceFailure,
    identity_access::workspace_name::CanonicalWorkspaceName,
    identity_access::workspace_name_availability::{HeldName, RegularWorkspaceNamespace},
};
use sqlx::{Row, SqlitePool};
use std::time::Duration;
use thiserror::Error;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The holder-of-a-key lookup. `name_key = ?` alone already excludes agent-home
/// rows (their key is `NULL` and `NULL = ?` is never true), but the namespace is
/// spelled out with `owner_agent_id IS NULL` so the SQL states the definition
/// rather than leaning on that coincidence — and matches the partial unique index
/// the create path collides against. At most one row can match (the index is
/// unique over regular workspaces), so `LIMIT 1` is a statement of that fact.
const SELECT_HOLDER_OF_NAME_KEY: &str = "\
SELECT name_key FROM workspaces \
 WHERE name_key = ? AND owner_agent_id IS NULL \
 LIMIT 1";

#[derive(Clone)]
pub struct SqliteRegularWorkspaceNamespace {
    pool: SqlitePool,
}

/// Read failures surfaced only as the source of a [`PersistenceFailure`]. The one
/// variant is a driver fault; there is no committed-row corruption arm because the
/// query projects a single already-validated `name_key` string.
#[derive(Debug, Error)]
pub enum WorkspaceNameNamespaceError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
}

impl SqliteRegularWorkspaceNamespace {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, WorkspaceNameNamespaceError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self { pool })
    }

    /// Construct over an already-open pool, so a composition root that shares one
    /// pool across adapters need not reopen the file.
    pub fn from_pool(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

impl RegularWorkspaceNamespace for SqliteRegularWorkspaceNamespace {
    async fn holder_of_name(
        &self,
        name: &CanonicalWorkspaceName,
    ) -> Result<Option<HeldName>, PersistenceFailure> {
        let row = sqlx::query(SELECT_HOLDER_OF_NAME_KEY)
            .bind(name.key())
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| {
                PersistenceFailure::from_source(WorkspaceNameNamespaceError::Sql(error))
            })?;
        // The projected column is the row's OWN key. It is returned verbatim; the
        // driver-free probe binds it against the key it asked for, so a widened
        // query that ever answered with a different key would be caught there.
        Ok(row.map(|row| HeldName {
            name_key: row.get::<String, _>("name_key"),
        }))
    }
}
