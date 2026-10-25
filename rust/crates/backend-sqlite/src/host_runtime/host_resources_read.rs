//! SQLite adapter for the host-resources cold read.
//!
//! It supplies the facts [`silverhand_backend_core::host_runtime::host_resources_read`] declares:
//! the computer's ownership row and the agents registered on it, resolved on ONE
//! `BEGIN DEFERRED` snapshot so the ownership a requester is judged against and the
//! rows listed to the wire come from one view of the database.
//!
//! ## The computer row is read UNFILTERED
//!
//! The product route resolves the computer with a bare `SELECT * FROM hosts WHERE
//! id = ?` and gates on ownership alone, so a revoked or not-yet-finalized computer
//! still answers its owner. Adding a lifecycle predicate here would be a new rule,
//! not a port — the faces that DO short-circuit on `revoked_at` own that themselves.
//!
//! ## Both statements are driven by the SAME bound coordinate
//!
//! One `HostId` is bound to the ownership select and to the agents select, so the
//! rows cannot come from two different computers. The core bundle re-checks it
//! anyway, on the row's own decoded `host_id`: that check is the structural seal,
//! this is just the honest way to ask.
//!
//! ## Every column that carries a contract is decoded strictly
//!
//! Not only the wire fields: a column is decoded strictly when something depends on
//! its value, and one of them is never rendered at all.
//!
//! - Ids go through the domain's fallible parsers. A stored value that cannot be a
//!   legal identity is corruption of a committed row and surfaces as a
//!   [`PersistenceFailure`] (a safe `500`), never coerced to a default or silently
//!   treated as an unowned computer.
//! - `runtime` is parsed into the closed [`RuntimeKind`] set the wire contract names.
//!   An unknown stored token is corruption too: coercing it — or passing it through
//!   as text — would put a value the wire enum cannot carry into a `200`.
//! - `created_at` is the AUTHORITY for the order this adapter contracts, so it is
//!   selected and decoded even though it is not a wire field. SQLite's column
//!   affinity lets a non-integer sit in `INTEGER NOT NULL`, where it silently joins a
//!   different, cross-type ordering; and a value outside the range a real instant can
//!   take was not written by this system's clock. Either way the order is no longer
//!   the contracted one, so the read fails rather than answering a plausible `200` in
//!   an arbitrary order. The value itself is then dropped: it is a key, not a fact
//!   this face reports.
//!
//! The two free-text columns pass through verbatim, exactly as the product's
//! `toHostAgentResource` copies them.

use std::{path::Path, time::Duration};

use silverhand_backend_core::{
    foundation::identity::{AgentId, HostId, UserId},
    foundation::persistence::PersistenceFailure,
    host_runtime::host_resources_read::{
        HostAgentResourceFact, HostResourcesFacts, HostResourcesReadModel,
    },
    identity_access::agents::runtime::RuntimeKind,
    identity_access::capability::computer_access::ComputerOwnershipFacts,
};
use sqlx::{Row, SqlitePool};
use thiserror::Error;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const DEFERRED_READ_BEGIN: &str = "BEGIN DEFERRED";

/// The two columns the ownership rule reads. Deliberately narrow: a computer row
/// that grows a lifecycle or a billing column cannot become an input to an
/// authorization decision by accident.
const SELECT_COMPUTER_OWNERSHIP: &str = "SELECT id, user_id FROM hosts WHERE id = ?";

/// Every agent filed under the computer, in the product's order. No stage
/// predicate: this is the registration view, so a pending or failed agent is part
/// of it. `created_at` is selected because it is what the order is contracted on —
/// a key this adapter must be able to trust, even though no field of the wire
/// carries it.
const SELECT_HOST_AGENT_RESOURCES: &str = "SELECT id, host_id, name, display_name, runtime, created_at FROM agents \
     WHERE host_id = ? ORDER BY created_at ASC";

/// The largest epoch a stored instant may hold: `2^53 - 1`, the last integer an
/// IEEE-754 double represents exactly. Named here rather than borrowed from a face
/// that renders its own timestamp — the bound is a property of the column, and this
/// read only needs it to tell a real clock's value from corruption.
const MAX_STORED_EPOCH: i64 = 9_007_199_254_740_991;

#[derive(Clone)]
pub struct SqliteHostResourcesColdRead {
    pool: SqlitePool,
}

/// Read failures surfaced only as the source of a [`PersistenceFailure`]. Every
/// variant is a driver fault or corruption of a committed row, never a
/// product-level refusal — that decision belongs to the core ownership rule.
#[derive(Debug, Error)]
pub enum HostResourcesColdReadError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored column {column} does not name a usable identity")]
    MalformedIdentity { column: &'static str },
    #[error("stored column {column} holds a token outside the closed set the wire carries")]
    MalformedToken { column: &'static str },
    #[error("stored column {column} does not hold an instant a clock could have written")]
    MalformedInstant { column: &'static str },
}

impl SqliteHostResourcesColdRead {
    pub fn from_pool(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn open(path: impl AsRef<Path>) -> Result<Self, HostResourcesColdReadError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self { pool })
    }

    async fn read_host_resources_facts(
        &self,
        computer: &HostId,
    ) -> Result<Option<HostResourcesFacts>, HostResourcesColdReadError> {
        let mut tx = self.pool.begin_with(DEFERRED_READ_BEGIN).await?;

        let ownership_row = crate::foundation::statement_metrics::statement(
            "hosts.get_resource_ownership",
            "get",
            sqlx::query(SELECT_COMPUTER_OWNERSHIP)
                .bind(computer.as_str())
                .fetch_optional(&mut *tx),
        )
        .await?;
        let Some(ownership_row) = ownership_row else {
            tx.commit().await?;
            return Ok(None);
        };
        let agent_rows = crate::foundation::statement_metrics::statement(
            "hosts.list_agent_resources",
            "all",
            sqlx::query(SELECT_HOST_AGENT_RESOURCES)
                .bind(computer.as_str())
                .fetch_all(&mut *tx),
        )
        .await?;

        tx.commit().await?;

        let id: String = ownership_row.try_get("id")?;
        let id = HostId::parse(id)
            .map_err(|_| HostResourcesColdReadError::MalformedIdentity { column: "hosts.id" })?;
        // Null is the computer no user owns, which the rule refuses like any other
        // stranger's. A non-null value that is not a legal id is corruption, and
        // must not degrade into that same refusal.
        let owner: Option<String> = ownership_row.try_get("user_id")?;
        let owner = owner.map(UserId::parse).transpose().map_err(|_| {
            HostResourcesColdReadError::MalformedIdentity {
                column: "hosts.user_id",
            }
        })?;

        let mut agents = Vec::with_capacity(agent_rows.len());
        for row in &agent_rows {
            let agent_id: String = row.try_get("id")?;
            let host_id: String = row.try_get("host_id")?;
            let runtime: String = row.try_get("runtime")?;
            // The ordering key is verified and then dropped. Only a decode failure
            // means the stored value is not an integer at all — affinity lets one sit
            // in `INTEGER NOT NULL`. Any other failure would be this file's own SELECT
            // being wrong, which stays a driver fault rather than corruption.
            let created_at: i64 = match row.try_get("created_at") {
                Ok(value) => value,
                Err(sqlx::Error::ColumnDecode { .. }) => {
                    return Err(HostResourcesColdReadError::MalformedInstant {
                        column: "agents.created_at",
                    });
                }
                Err(error) => return Err(error.into()),
            };
            if !(0..=MAX_STORED_EPOCH).contains(&created_at) {
                return Err(HostResourcesColdReadError::MalformedInstant {
                    column: "agents.created_at",
                });
            }
            agents.push(HostAgentResourceFact {
                id: AgentId::parse(agent_id).map_err(|_| {
                    HostResourcesColdReadError::MalformedIdentity {
                        column: "agents.id",
                    }
                })?,
                computer: HostId::parse(host_id).map_err(|_| {
                    HostResourcesColdReadError::MalformedIdentity {
                        column: "agents.host_id",
                    }
                })?,
                name: row.try_get("name")?,
                display_name: row.try_get("display_name")?,
                runtime: RuntimeKind::parse(&runtime).ok_or(
                    HostResourcesColdReadError::MalformedToken {
                        column: "agents.runtime",
                    },
                )?,
            });
        }

        Ok(Some(HostResourcesFacts::from_ownership(
            ComputerOwnershipFacts { id, owner },
            agents,
        )))
    }
}

impl HostResourcesReadModel for SqliteHostResourcesColdRead {
    async fn read_host_resources(
        &self,
        computer: &HostId,
    ) -> Result<Option<HostResourcesFacts>, PersistenceFailure> {
        self.read_host_resources_facts(computer)
            .await
            .map_err(PersistenceFailure::from_source)
    }
}

#[cfg(test)]
mod tests {
    use silverhand_backend_core::host_runtime::host_resources_read::visible_host_resources;
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::{Connection, Executor};

    use super::*;
    use crate::current_schema;

    /// Two users, two computers, and agents whose insertion order is deliberately
    /// NOT their `created_at` order, so a missing `ORDER BY` cannot pass by luck.
    /// `a-late` also proves the registration view keeps a non-active stage.
    const SEED: &str = "\
        INSERT INTO users (id, name, display_name, created_at, updated_at) \
        VALUES ('u-owner', 'owner', 'Owner', 1, 1), ('u-other', 'other', 'Other', 1, 1); \
        INSERT INTO hosts (id, name, kind, created_at, user_id) \
        VALUES ('h-owned', 'owned', 'desktop', 1, 'u-owner'), \
               ('h-theirs', 'theirs', 'desktop', 1, 'u-other'); \
        INSERT INTO hosts (id, name, kind, created_at) \
        VALUES ('h-ownerless', 'ownerless', 'desktop', 1); \
        INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, display_name, \
          created_by_user_id, materialization_status) \
        VALUES ('a-late', 'late', 'codex', 'h-owned', 300, 300, 'Late', 'u-owner', 'failed'), \
               ('a-early', 'early', 'qoder', 'h-owned', 100, 100, 'Early', 'u-owner', 'active'), \
               ('a-elsewhere', 'elsewhere', 'qoder', 'h-theirs', 100, 100, 'Elsewhere', \
                'u-other', 'active');";

    async fn seeded() -> (tempfile::TempDir, SqliteHostResourcesColdRead) {
        seeded_with("").await
    }

    const DATABASE_FILE: &str = "host-resources.sqlite";

    /// Materialize the fixture, then apply `extra` on the SAME owned connection.
    /// Fixture mutations belong here rather than through the adapter's pool: the
    /// pool hands out an arbitrary connection, which makes a connection-scoped
    /// `PRAGMA` unreliable, and a read adapter is not a write path.
    async fn seeded_with(extra: &'static str) -> (tempfile::TempDir, SqliteHostResourcesColdRead) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(DATABASE_FILE);
        let mut db = current_schema::create_current_schema(&path).await;
        db.execute(sqlx::raw_sql(SEED))
            .await
            .expect("seed must load against the current schema");
        if !extra.is_empty() {
            db.execute(sqlx::raw_sql(extra))
                .await
                .expect("fixture mutation must apply");
        }
        db.close().await.unwrap();
        let cold_read = SqliteHostResourcesColdRead::open(&path).await.unwrap();
        (directory, cold_read)
    }

    /// Apply a statement through a connection the test owns while the adapter is
    /// already open. A repair has to be observable through the SAME adapter: a
    /// freshly built one would prove the fixture changed, not that the read
    /// recovers.
    async fn repair(directory: &tempfile::TempDir, statement: &'static str) {
        let options = SqliteConnectOptions::new().filename(directory.path().join(DATABASE_FILE));
        let mut db = sqlx::SqliteConnection::connect_with(&options)
            .await
            .expect("the fixture must stay writable");
        db.execute(sqlx::raw_sql(statement))
            .await
            .expect("repair must apply");
        db.close().await.unwrap();
    }

    fn host(id: &str) -> HostId {
        HostId::parse(id).expect("host id")
    }

    /// The agent ids `viewer` may see on `computer`, through the core gate — the only
    /// path to the list, so the adapter is exercised the way a route reaches it.
    async fn visible(
        cold_read: &SqliteHostResourcesColdRead,
        viewer: &str,
        computer: &str,
    ) -> Option<Vec<String>> {
        let computer = host(computer);
        let facts = cold_read.read_host_resources(&computer).await.unwrap();
        visible_host_resources(&UserId::parse(viewer).unwrap(), &computer, facts.as_ref())
            .expect("the adapter must resolve facts about the computer it was asked for")
            .map(|agents| {
                agents
                    .iter()
                    .map(|agent| agent.id.as_str().to_string())
                    .collect()
            })
    }

    /// The order is this adapter's contract: `created_at ASC`, and the stage is not
    /// filtered, so a failed agent stays in the registration view.
    #[tokio::test]
    async fn the_owner_sees_this_computers_agents_oldest_first_at_any_stage() {
        let (_directory, cold_read) = seeded().await;
        assert_eq!(
            visible(&cold_read, "u-owner", "h-owned").await,
            Some(vec!["a-early".to_string(), "a-late".to_string()])
        );
        // The neighboring computer must resolve its own set from the same adapter.
        assert_eq!(
            visible(&cold_read, "u-other", "h-theirs").await,
            Some(vec!["a-elsewhere".to_string()])
        );
    }

    /// An owned computer with no agents is an empty list, not the refusal an absent
    /// or foreign computer gets.
    #[tokio::test]
    async fn an_owned_computer_with_no_agents_answers_with_an_empty_list() {
        let (_directory, cold_read) =
            seeded_with("DELETE FROM agents WHERE host_id = 'h-owned';").await;
        assert_eq!(
            visible(&cold_read, "u-owner", "h-owned").await,
            Some(vec![])
        );
    }

    /// What the ownership column decides, read from real rows: a stranger, an absent
    /// computer and one no user owns are all the same nothing.
    #[tokio::test]
    async fn a_stranger_an_absent_computer_and_an_ownerless_one_all_see_nothing() {
        let (_directory, cold_read) = seeded().await;
        assert_eq!(visible(&cold_read, "u-other", "h-owned").await, None);
        assert_eq!(visible(&cold_read, "u-owner", "h-missing").await, None);
        assert_eq!(visible(&cold_read, "u-owner", "h-ownerless").await, None);
    }

    /// A revoked computer still answers its owner: this face gates on ownership
    /// alone, and adding a lifecycle predicate here would invent a rule the product
    /// route does not apply.
    #[tokio::test]
    async fn a_revoked_computer_still_answers_its_owner() {
        let (_directory, cold_read) =
            seeded_with("UPDATE hosts SET revoked_at = 999 WHERE id = 'h-owned';").await;
        assert_eq!(
            visible(&cold_read, "u-owner", "h-owned").await,
            Some(vec!["a-early".to_string(), "a-late".to_string()])
        );
    }

    /// The four wire-shaped strings are copied verbatim off the row, including an
    /// empty `display_name` (the schema default) — no coalescing, which the product
    /// projection does not do either.
    #[tokio::test]
    async fn the_projected_strings_are_the_stored_columns_verbatim() {
        let (_directory, cold_read) =
            seeded_with("UPDATE agents SET display_name = '' WHERE id = 'a-early';").await;
        let computer = host("h-owned");
        let facts = cold_read
            .read_host_resources(&computer)
            .await
            .unwrap()
            .expect("the computer row exists");
        let agents =
            visible_host_resources(&UserId::parse("u-owner").unwrap(), &computer, Some(&facts))
                .unwrap()
                .expect("the owner is admitted");
        let early = &agents[0];
        assert_eq!(early.id.as_str(), "a-early");
        assert_eq!(early.name, "early");
        assert_eq!(early.display_name, "");
        assert_eq!(early.runtime, RuntimeKind::Qoder);
        assert_eq!(early.computer, computer);
    }

    /// A committed id that cannot be a legal identity is corruption, and must not
    /// degrade into "no owner" — which would answer `404` and hide it.
    #[tokio::test]
    async fn a_malformed_stored_owner_is_a_read_failure_not_an_unowned_computer() {
        let (_directory, cold_read) = seeded_with(
            "PRAGMA foreign_keys = OFF; UPDATE hosts SET user_id = '' WHERE id = 'h-owned';",
        )
        .await;
        let error = cold_read
            .read_host_resources_facts(&host("h-owned"))
            .await
            .expect_err("an empty stored owner is not a usable identity");
        assert!(matches!(
            error,
            HostResourcesColdReadError::MalformedIdentity {
                column: "hosts.user_id"
            }
        ));
    }

    /// `agents.runtime` carries no CHECK, so an unknown token is storable — and it is
    /// corruption, not a runtime. Passing it through would answer `200` with a value
    /// the wire enum cannot name, so the whole read fails; a repair inside the closed
    /// set is projected as ITSELF, which is what rules out a coerced default.
    #[tokio::test]
    async fn an_unknown_stored_runtime_fails_the_read_until_it_is_repaired() {
        let (directory, cold_read) =
            seeded_with("UPDATE agents SET runtime = 'unknown' WHERE id = 'a-early';").await;
        let error = cold_read
            .read_host_resources_facts(&host("h-owned"))
            .await
            .expect_err("a token outside the closed set is not a runtime");
        assert!(matches!(
            error,
            HostResourcesColdReadError::MalformedToken {
                column: "agents.runtime"
            }
        ));

        repair(
            &directory,
            "UPDATE agents SET runtime = 'cursor' WHERE id = 'a-early';",
        )
        .await;
        let computer = host("h-owned");
        let facts = cold_read
            .read_host_resources(&computer)
            .await
            .expect("the repaired row decodes")
            .expect("the computer row exists");
        let agents =
            visible_host_resources(&UserId::parse("u-owner").unwrap(), &computer, Some(&facts))
                .unwrap()
                .expect("the owner is admitted");
        assert_eq!(agents[0].runtime, RuntimeKind::Cursor);
    }

    /// `created_at` is the authority for the order this adapter contracts. Affinity
    /// lets a non-integer sit in `INTEGER NOT NULL`, where it joins SQLite's
    /// cross-type ordering instead, and an out-of-range stamp is not a value a clock
    /// wrote: both would answer `200` in an order the contract does not describe.
    /// Decoding the key strictly makes each a read failure, and repairing the stamp
    /// restores oldest-first.
    #[tokio::test]
    async fn an_unusable_ordering_key_fails_the_read_until_it_is_repaired() {
        let (directory, cold_read) =
            seeded_with("UPDATE agents SET created_at = 'bad' WHERE id = 'a-early';").await;
        let error = cold_read
            .read_host_resources_facts(&host("h-owned"))
            .await
            .expect_err("a non-integer key cannot be trusted to order the list");
        assert!(matches!(
            error,
            HostResourcesColdReadError::MalformedInstant {
                column: "agents.created_at"
            }
        ));

        // 9_007_199_254_740_992 == MAX_STORED_EPOCH + 1: a legal integer, refused for
        // being past the range a stored instant may hold.
        repair(
            &directory,
            "UPDATE agents SET created_at = 9007199254740992 WHERE id = 'a-early';",
        )
        .await;
        let error = cold_read
            .read_host_resources_facts(&host("h-owned"))
            .await
            .expect_err("a stamp past the wire-safe range is corruption too");
        assert!(matches!(
            error,
            HostResourcesColdReadError::MalformedInstant {
                column: "agents.created_at"
            }
        ));

        repair(
            &directory,
            "UPDATE agents SET created_at = 100 WHERE id = 'a-early';",
        )
        .await;
        assert_eq!(
            visible(&cold_read, "u-owner", "h-owned").await,
            Some(vec!["a-early".to_string(), "a-late".to_string()])
        );
    }
}
