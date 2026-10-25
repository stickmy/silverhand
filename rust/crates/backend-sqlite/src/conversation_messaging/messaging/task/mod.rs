//! SQLite adapter for the task board.
//!
//! This store CONSUMES the product database. It creates no table, runs no
//! migrator, claims no schema version, and refuses to open a database that does
//! not exist: the `tasks` table has an owner, and a second migration authority
//! would be one too many.
//!
//! This module is the ASSEMBLY: it owns the store handle (`SqliteTaskStore`),
//! its connection setup, and the store-level failure vocabulary
//! (`TaskStoreError` and the two erasing helpers that route a driver error and
//! a decode corruption to their distinct safe-500 sources). Everything else lives
//! in a leaf with a single reason to change:
//!
//!  - `row` — the committed-row decode and the cross-row board seal every read
//!    shares (what a trustworthy `tasks` row IS), and the shared row projection;
//!  - `point_read` — the single-task fact keyed by `tasks.id`;
//!  - `board_list` — the [`TaskCandidateFetch`](silverhand_backend_core::conversation_messaging::messaging::task::TaskCandidateFetch)
//!    adapter: the plural candidate fetch a board list trims (coordinate filters
//!    only; participation, ordering, and limit are the core's).
//!  - `human_board_read` — the public board/count/facet read model, with human
//!    placement visibility inside each SQL query before pagination;
//!  - `point_detail` — viewer-independent task-detail sidecars, board lookup,
//!    message navigation candidates, and last-editor display resolution.
//!
//! Write units-of-work each land as their own leaves; consumer-defined ports and
//! wire adapters remain separate slices too.

mod board_list;
mod claim_write;
mod create_admission;
mod create_write;
mod edit_write;
mod event_append;
mod human_board_read;
mod point_detail;
mod point_read;
mod row;
mod status_write;
mod write_gate;

pub use claim_write::SqliteTaskClaimWrite;
pub use create_write::SqliteTaskCreateWrite;
pub use edit_write::SqliteTaskEditWrite;
pub use status_write::SqliteTaskStatusWrite;

use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use silverhand_backend_core::conversation_messaging::messaging::task::TaskWriteClock;
use silverhand_backend_core::foundation::persistence::PersistenceFailure;
use thiserror::Error;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemTaskWriteClock;

impl TaskWriteClock for SystemTaskWriteClock {
    fn now(&self) -> i64 {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or(0);
        i64::try_from(millis).unwrap_or(i64::MAX)
    }
}

/// Startup and driver failures, surfaced only to the composition root that opens
/// the store, plus the decode-corruption reasons a broken persisted row carries to
/// a safe-500. The port returns `PersistenceFailure`, so none of these spellings
/// reach the application or the wire.
#[derive(Debug, Error)]
pub enum TaskStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    /// A stored `tasks` row (or its board's agreement with the conversation
    /// placement) is outside a contract a correct writer upholds. Reported with the
    /// offending column so an operator can find it; it is corruption, never a
    /// caller's mistake.
    #[error("stored task column {field} is outside the domain contract")]
    Corrupt { field: &'static str },
    /// The OS entropy source failed while minting a new task/message id. Surfaced
    /// to the caller as a `PersistenceFailure`; the create wrote nothing.
    #[error("secure random source failed")]
    Entropy(#[source] getrandom::Error),
}

/// Erase a driver read error into the port's safe-500. Private to the module, so
/// each read leaf routes its `sqlx` failures through the same source.
fn driver(error: sqlx::Error) -> PersistenceFailure {
    tracing::error!(
        stage = "task_store",
        failure_kind = "database",
        "task persistence failed"
    );
    PersistenceFailure::from_source(TaskStoreError::Sql(error))
}

/// Erase a named corruption reason into the port's safe-500, preserving the
/// offending column so an operator can find it.
fn corrupt(field: &'static str) -> PersistenceFailure {
    tracing::error!(
        stage = "task_decode",
        field,
        "stored task violates domain contract"
    );
    PersistenceFailure::from_source(TaskStoreError::Corrupt { field })
}

/// The SQLite task store. A read has no clock and no bookkeeping writes, so unlike
/// the schedule store it carries no injected clock.
#[derive(Clone)]
pub struct SqliteTaskStore {
    pool: crate::foundation::database::SqliteDatabase,
}

impl SqliteTaskStore {
    pub fn from_pool(pool: impl Into<crate::foundation::database::SqliteDatabase>) -> Self {
        let pool = pool.into();
        Self { pool }
    }

    /// Opens an existing product database. A missing file is a failure rather than
    /// an empty database: the schema has an owner, and this store is not it.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, TaskStoreError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;

        Ok(Self { pool })
    }
}
