//! SQLite adapter for the product schedule LIST read
//! (`GET /api/agents/{id}/schedules`).
//!
//! A read-only projection over the product database: it opens no write
//! transaction, claims no schema version, and shares NOTHING with the fire-path
//! claim/settlement store beyond the reused row decode. The status family is a
//! bound `IN (...)` set (no dynamic SQL beyond the placeholder count), the target
//! label rides a LEFT JOIN to `conversations`, and the order is
//! `next_fire_at ASC, created_at ASC` capped at the caller's limit — the exact
//! `listForAgent` contract.

use std::{path::Path, time::Duration};

use silverhand_backend_core::conversation_messaging::messaging::schedule::{
    ScheduleListQuery, ScheduleListReadModel, ScheduleView,
};
use silverhand_backend_core::foundation::persistence::PersistenceFailure;
use sqlx::{QueryBuilder, Sqlite};

use super::product::project_schedule_view;
use super::{ScheduleStoreError, driver};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The projection fragment, joining the target conversation for the label. Kept
/// byte-for-byte aligned with `SCHEDULE_SELECT_WITH_TARGET`: `s.*` plus the two
/// label columns, LEFT JOIN so a deleted or unresolved target yields NULLs (→
/// `"Unavailable conversation"`) rather than dropping the row.
const SELECT_WITH_TARGET: &str = "SELECT s.*, \
     tc.title AS target_title, tc.roster_policy AS target_roster_policy \
     FROM schedules s LEFT JOIN conversations tc ON tc.id = s.target_conversation_id \
     WHERE s.owner_agent_id = ";

/// The read side of the schedule list. Holds only a pool; it is its OWN sole
/// authority, never fused with the fire-path store.
#[derive(Clone)]
pub struct SqliteScheduleListRead {
    pool: crate::foundation::database::SqliteDatabase,
}

impl SqliteScheduleListRead {
    /// Opens an existing product database. A missing file is a failure, not an
    /// empty database: the schema has an owner, and this reader is not it.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, ScheduleStoreError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self { pool })
    }

    /// Wraps an already-open pool, so a composition root can share ONE pool per
    /// database across the schedule faces.
    pub fn from_pool(pool: impl Into<crate::foundation::database::SqliteDatabase>) -> Self {
        let pool = pool.into();
        Self { pool }
    }
}

impl ScheduleListReadModel for SqliteScheduleListRead {
    async fn list_owned_schedules(
        &self,
        query: &ScheduleListQuery,
    ) -> Result<Vec<ScheduleView>, PersistenceFailure> {
        // An empty status set can never match — the core would only produce this
        // through an explicitly empty expansion, but short-circuit rather than
        // emit `IN ()`, which is a SQL error.
        if query.statuses.as_ref().is_some_and(|s| s.is_empty()) {
            return Ok(Vec::new());
        }

        let mut builder: QueryBuilder<Sqlite> = QueryBuilder::new(SELECT_WITH_TARGET);
        builder.push_bind(query.owner_agent_id.as_str().to_owned());
        if let Some(statuses) = &query.statuses {
            builder.push(" AND s.status IN (");
            let mut separated = builder.separated(", ");
            for status in statuses {
                separated.push_bind(status.as_token());
            }
            separated.push_unseparated(")");
        }
        builder.push(" ORDER BY s.next_fire_at ASC, s.created_at ASC LIMIT ");
        builder.push_bind(query.limit.get());

        // The target join and owner filter are in this one statement's snapshot.
        let rows = builder
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(driver)?;

        rows.iter()
            .map(|row| {
                let target_title: Option<String> =
                    sqlx::Row::try_get(row, "target_title").map_err(driver)?;
                let target_roster_policy: Option<String> =
                    sqlx::Row::try_get(row, "target_roster_policy").map_err(driver)?;
                project_schedule_view(
                    row,
                    target_title.as_deref(),
                    target_roster_policy.as_deref(),
                )
            })
            .collect()
    }
}
