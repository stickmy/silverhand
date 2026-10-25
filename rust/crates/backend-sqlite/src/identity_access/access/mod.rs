//! SQLite adapter for workspace enrollment and the governance door in front of
//! it.
//!
//! Unlike the foundation proof store, this is a **pure consumer** of an existing
//! product database: it opens a pool against a schema that already exists and
//! creates, migrates, or seeds no schema of its own. It therefore never takes
//! over a database's schema identity — it only reads the `agents`, `workspaces`
//! and `users` rows and writes the `workspace_members` /
//! `workspace_agent_capabilities` / `workspace_users` edges the product schema
//! already defines.
//!
//! It decides nothing. Both authorization doors it serves are rules in the
//! application layer: the request-boundary verdict is
//! [`governance_ceiling`] over the facts this adapter reads, and the in-lock
//! re-derivation is `human_enrollment_authorized` over the role it re-reads under
//! the write lock. Neither role axis is re-implemented here, so the two doors
//! cannot drift apart in this crate.
//!
//! The whole enrollment is one `BEGIN IMMEDIATE` transaction. The membership
//! insert is `INSERT OR IGNORE`, so a re-enrollment is a no-op on the row;
//! `created` is read from that insert's own affected-row count, and the
//! create-only capability seed runs only when it created the row. The join-time
//! candidate is *not* sampled here — the application samples its clock after
//! authorization and hands the candidate in, so a re-enrollment keeps the stored
//! value and the time source stays in the application. This crate owns no clock
//! at all: the [`EnrollmentClock`](silverhand_backend_core::identity_access::access::EnrollmentClock)
//! port and its `Date.now()`-parity semantics live in the domain, and the concrete
//! host-wall-clock adapter is assembled in the application layer, because reading
//! the host wall clock is not persistence. Every driver error is handed to the
//! port's caller as the source of a `PersistenceFailure`; nothing is logged here.

use std::{path::Path, time::Duration};

use silverhand_backend_core::foundation::identity::{AgentId, Principal, UserId, WorkspaceId};
use silverhand_backend_core::foundation::persistence::PersistenceFailure;
use silverhand_backend_core::identity_access::access::{
    AddHumanMemberError, AgentAuthorityFacts, AgentEnrollmentReadPort, AgentEnrollmentUnitOfWork,
    AgentWorkspaceMembership, Capability, EnrollmentKey, HumanEnrollmentCommit, HumanEnrollmentKey,
    HumanEnrollmentReadPort, HumanEnrollmentUnitOfWork, HumanUser, WorkspaceAccessPolicy,
    WorkspaceGovernance, WorkspaceGovernanceFacts, WorkspaceGovernanceFactsRead, WorkspaceRole,
    WorkspaceUserMembership, governance_ceiling, human_enrollment_authorized,
};
use silverhand_backend_core::identity_access::agent_peer_profile::PeerTargetMembership;
use thiserror::Error;

mod membership;

const ROOT_WRITE_BEGIN: &str = "BEGIN IMMEDIATE";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const SELECT_AGENT_AUTHORITY: &str = "SELECT created_by_user_id FROM agents WHERE id = ?";
const INSERT_MEMBERSHIP: &str =
    "INSERT OR IGNORE INTO workspace_members (workspace_id, agent_id, joined_at) VALUES (?, ?, ?)";
const SELECT_MEMBERSHIP_JOINED_AT: &str =
    "SELECT joined_at FROM workspace_members WHERE workspace_id = ? AND agent_id = ?";
const INSERT_CAPABILITY: &str = "INSERT OR IGNORE INTO workspace_agent_capabilities \
     (workspace_id, agent_id, capability) VALUES (?, ?, ?)";

// The peer-profile target read's membership edge: EXACTLY the regular
// `workspace_members` pair, nothing wider. An agent's private home (a
// `workspaces.owner_agent_id` row) can never carry a `workspace_members` entry —
// the schema's `workspace_members_no_agent_home_insert` trigger aborts one — so
// this answers `false` for the home even when the caller's workspace horizon can
// see it, keying purely on the regular edge. Parity with the TS
// `workspaces.isMember` COUNT read.
const SELECT_AGENT_IS_WORKSPACE_MEMBER: &str =
    "SELECT EXISTS(SELECT 1 FROM workspace_members WHERE workspace_id = ? AND agent_id = ?)";

// Human enrollment writes the `workspace_users` roster row rather than
// `workspace_members`. The actor's role is re-read under the same lock to
// re-derive governance; the target row is `INSERT OR IGNORE`d at the default
// `member` role and then re-read, so a re-enrollment keeps the original role
// (possibly `owner`) and join time.
const SELECT_WORKSPACE_USER_ROLE: &str =
    "SELECT role FROM workspace_users WHERE workspace_id = ? AND user_id = ?";
const INSERT_HUMAN_MEMBERSHIP: &str = "INSERT OR IGNORE INTO workspace_users \
     (workspace_id, user_id, joined_at, role) VALUES (?, ?, ?, 'member')";
const SELECT_HUMAN_MEMBERSHIP: &str =
    "SELECT joined_at, role FROM workspace_users WHERE workspace_id = ? AND user_id = ?";
const SELECT_USER: &str = "SELECT id, name, display_name, avatar_storage_key, created_at, \
     updated_at FROM users WHERE id = ?";

// The request-boundary governance door. `owner_agent_id` is the authority on
// whether a row is some agent's private home (never a stored kind word), so it
// is selected rather than inferred, and the caller's role is left-joined in the
// SAME statement: the door's contract is that the container and the caller's
// ceiling are resolved together, in one read, before any enrollment work runs.
const SELECT_WORKSPACE_GOVERNANCE_FOR_HUMAN: &str = "SELECT w.owner_agent_id, u.role \
     FROM workspaces w \
     LEFT JOIN workspace_users u ON u.workspace_id = w.id AND u.user_id = ? \
     WHERE w.id = ?";
/// A non-human principal holds no role row, so its verdict needs only the
/// container fact — reading a role for it would be reading a row that by
/// definition cannot exist.
const SELECT_WORKSPACE_CONTAINER: &str = "SELECT owner_agent_id FROM workspaces WHERE id = ?";

/// A pooled connection to an existing product database, offering only the
/// agent-enrollment ports. It holds no clock: the join-time candidate is passed
/// into the unit of work by the application, which owns the time source.
#[derive(Clone)]
pub struct SqliteAccessStore {
    pool: crate::foundation::database::SqliteDatabase,
}

/// Startup and driver failures, surfaced only to the composition root that opens
/// the store. The ports below hand the driver error over as a source instead.
#[derive(Debug, Error)]
pub enum SqliteAccessStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
}

/// A `workspace_users.role` value outside the schema's `CHECK` set. Committed
/// rows cannot hold one, so this is a persistence-integrity fault surfaced as the
/// source of a [`PersistenceFailure`] — never silently coerced to a default.
#[derive(Debug, Error)]
#[error("workspace_users.role holds an unrecognized value: {0}")]
struct CorruptRoleToken(String);

/// A committed row whose identifier column decodes to a value the domain refuses
/// — today only the empty string, which `identity::parse` rejects. The product
/// schema's ids are non-empty by construction, so such a row is a
/// persistence-integrity fault: it is surfaced as the source of a
/// [`PersistenceFailure`] (a safe `500`), never coerced to a sentinel id and never
/// mistaken for the `404` that a *requested* empty id earns at the wire.
#[derive(Debug, Error)]
#[error("{table}.{column} holds an unusable identifier")]
struct CorruptStoredIdentifier {
    table: &'static str,
    column: &'static str,
}

/// A read that failed in the driver or on a row that cannot be decoded into the
/// domain. Both reach the caller as the source of one `PersistenceFailure`; the
/// split exists so an integrity fault names its exact column instead of
/// masquerading as a driver error.
#[derive(Debug, Error)]
enum AccessReadFailure {
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
    #[error(transparent)]
    Integrity(#[from] CorruptStoredIdentifier),
    #[error(transparent)]
    Role(#[from] CorruptRoleToken),
}

/// The internal outcome of the human-enrollment transaction. `Forbidden` is the
/// in-lock authority refusal (a rollback), kept distinct from the driver
/// failures so the port maps it to the typed [`AddHumanMemberError::Forbidden`]
/// rather than a `500`.
enum HumanEnrollFailure {
    Forbidden,
    Sql(sqlx::Error),
    CorruptRole(String),
}

impl From<sqlx::Error> for HumanEnrollFailure {
    fn from(error: sqlx::Error) -> Self {
        Self::Sql(error)
    }
}

impl SqliteAccessStore {
    pub fn from_pool(pool: impl Into<crate::foundation::database::SqliteDatabase>) -> Self {
        let pool = pool.into();
        Self { pool }
    }

    pub async fn open(path: impl AsRef<Path>) -> Result<Self, SqliteAccessStoreError> {
        Self::open_with_busy_timeout(path, BUSY_TIMEOUT).await
    }

    /// Opens the pool as a consumer: `create_if_missing` is off, so a database
    /// that has never been migrated is a hard open error rather than a silently
    /// created empty file this adapter has no schema to fill.
    pub async fn open_with_busy_timeout(
        path: impl AsRef<Path>,
        busy_timeout: Duration,
    ) -> Result<Self, SqliteAccessStoreError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), busy_timeout).await?;

        Ok(Self { pool })
    }

    async fn agent_authority(
        &self,
        agent_id: &AgentId,
    ) -> Result<Option<AgentAuthorityFacts>, AccessReadFailure> {
        let created_by: Option<String> = sqlx::query_scalar(SELECT_AGENT_AUTHORITY)
            .bind(agent_id.as_str())
            .fetch_optional(&self.pool)
            .await?;
        let Some(user_id) = created_by else {
            return Ok(None);
        };
        // A stored owner id the domain refuses is corruption of a committed row,
        // not a product answer, so it fails the read rather than resolving the
        // agent to an unusable owner.
        let created_by_user_id = UserId::parse(user_id).map_err(|_| CorruptStoredIdentifier {
            table: "agents",
            column: "created_by_user_id",
        })?;
        Ok(Some(AgentAuthorityFacts {
            id: agent_id.clone(),
            created_by_user_id,
        }))
    }

    /// Reads exactly the facts the pure boundary rule needs. It decides nothing:
    /// the ceiling is derived by [`governance_ceiling`] in the application layer,
    /// so this adapter never re-implements the role axis.
    async fn governance_facts(
        &self,
        principal: &Principal,
        workspace_id: &WorkspaceId,
    ) -> Result<WorkspaceGovernanceFacts, AccessReadFailure> {
        let (owner_agent_id, role_token) = match principal {
            Principal::Human { user_id } => {
                let row: Option<(Option<String>, Option<String>)> =
                    sqlx::query_as(SELECT_WORKSPACE_GOVERNANCE_FOR_HUMAN)
                        .bind(user_id.as_str())
                        .bind(workspace_id.as_str())
                        .fetch_optional(&self.pool)
                        .await?;
                match row {
                    Some((owner_agent_id, role_token)) => (owner_agent_id, role_token),
                    None => return Ok(WorkspaceGovernanceFacts::NoSuchWorkspace),
                }
            }
            Principal::System | Principal::Agent { .. } => {
                let row: Option<Option<String>> = sqlx::query_scalar(SELECT_WORKSPACE_CONTAINER)
                    .bind(workspace_id.as_str())
                    .fetch_optional(&self.pool)
                    .await?;
                match row {
                    Some(owner_agent_id) => (owner_agent_id, None),
                    None => return Ok(WorkspaceGovernanceFacts::NoSuchWorkspace),
                }
            }
        };

        // A row with a home agent is that agent's private container, which has no
        // roster to govern — the same "no such workspace" as a missing row, so the
        // caller cannot tell the two apart.
        if owner_agent_id.is_some() {
            return Ok(WorkspaceGovernanceFacts::NoSuchWorkspace);
        }

        // A committed role outside the schema's CHECK set is corruption, never a
        // silent "not a member": it fails the read as an integrity fault.
        let actor_role = match role_token {
            Some(token) => Some(WorkspaceRole::parse(&token).ok_or(CorruptRoleToken(token))?),
            None => None,
        };
        Ok(WorkspaceGovernanceFacts::Regular { actor_role })
    }

    #[tracing::instrument(skip(self, seed_on_create))]
    async fn enroll_agent_transaction(
        &self,
        key: EnrollmentKey,
        seed_on_create: &[Capability],
        joined_at_candidate: i64,
    ) -> Result<AgentWorkspaceMembership, SqliteAccessStoreError> {
        // Reserve the single writer before the first statement, so a concurrent
        // enrollment of the same pair serializes here rather than interleaving.
        let mut tx = self.pool.begin_with(ROOT_WRITE_BEGIN).await?;

        let insert = sqlx::query(INSERT_MEMBERSHIP)
            .bind(key.workspace_id.as_str())
            .bind(key.agent_id.as_str())
            .bind(joined_at_candidate)
            .execute(&mut *tx)
            .await?;
        let created = insert.rows_affected() == 1;

        // Seed capabilities only when this transaction created the membership, so
        // re-enrolling a member whose set was later narrowed never re-widens it.
        if created {
            for capability in seed_on_create {
                sqlx::query(INSERT_CAPABILITY)
                    .bind(key.workspace_id.as_str())
                    .bind(key.agent_id.as_str())
                    .bind(capability.as_str())
                    .execute(&mut *tx)
                    .await?;
            }
        }

        // On a re-enrollment the ignored insert kept the original, so the value
        // returned is always re-read from the row, never the candidate binding.
        let stored_joined_at: i64 = sqlx::query_scalar(SELECT_MEMBERSHIP_JOINED_AT)
            .bind(key.workspace_id.as_str())
            .bind(key.agent_id.as_str())
            .fetch_one(&mut *tx)
            .await?;

        tx.commit().await?;

        Ok(AgentWorkspaceMembership {
            workspace_id: key.workspace_id,
            agent_id: key.agent_id,
            joined_at_ms: stored_joined_at,
        })
    }

    async fn user_record(&self, user_id: &UserId) -> Result<Option<HumanUser>, AccessReadFailure> {
        let row: Option<(String, String, String, Option<String>, i64, i64)> =
            sqlx::query_as(SELECT_USER)
                .bind(user_id.as_str())
                .fetch_optional(&self.pool)
                .await?;
        let Some((id, name, display_name, avatar_storage_key, created_at, updated_at)) = row else {
            return Ok(None);
        };
        // Mirror `principalAvatarUrl`: a present, non-empty storage key yields a
        // versioned avatar path, otherwise there is no avatar.
        let avatar_url = match avatar_storage_key {
            Some(ref key) if !key.is_empty() => {
                Some(format!("/api/users/{id}/avatar?v={updated_at}"))
            }
            _ => None,
        };
        let id = UserId::parse(id).map_err(|_| CorruptStoredIdentifier {
            table: "users",
            column: "id",
        })?;
        Ok(Some(HumanUser {
            id,
            name,
            display_name,
            avatar_url,
            created_at,
            updated_at,
        }))
    }

    #[tracing::instrument(skip(self))]
    async fn enroll_human_transaction(
        &self,
        key: HumanEnrollmentKey,
        actor: Principal,
        joined_at_candidate: i64,
    ) -> Result<HumanEnrollmentCommit, HumanEnrollFailure> {
        // Reserve the single writer before the first statement, so the actor's
        // authority is re-read and the roster row written under one lock.
        let mut tx = self.pool.begin_with(ROOT_WRITE_BEGIN).await?;

        // In-lock authority re-derivation. Only a human principal has a role row
        // to read; `system` and an agent principal are decided by the rule
        // without one. A demotion that committed after the boundary gate is
        // visible here, so the write refuses instead of enrolling on stale
        // authority.
        let actor_role = if let Principal::Human { user_id } = &actor {
            let token: Option<String> = sqlx::query_scalar(SELECT_WORKSPACE_USER_ROLE)
                .bind(key.workspace_id.as_str())
                .bind(user_id.as_str())
                .fetch_optional(&mut *tx)
                .await?;
            match token {
                Some(token) => Some(
                    WorkspaceRole::parse(&token).ok_or(HumanEnrollFailure::CorruptRole(token))?,
                ),
                None => None,
            }
        } else {
            None
        };
        if !human_enrollment_authorized(&actor, actor_role) {
            // Dropping the transaction rolls it back; nothing was written.
            return Err(HumanEnrollFailure::Forbidden);
        }

        let insert = sqlx::query(INSERT_HUMAN_MEMBERSHIP)
            .bind(key.workspace_id.as_str())
            .bind(key.user_id.as_str())
            .bind(joined_at_candidate)
            .execute(&mut *tx)
            .await?;
        let created = insert.rows_affected() == 1;

        // Re-read the row: a re-enrollment kept the original role and join time,
        // so the member is never assumed from the candidate or the default.
        let (stored_joined_at, role_token): (i64, String) = sqlx::query_as(SELECT_HUMAN_MEMBERSHIP)
            .bind(key.workspace_id.as_str())
            .bind(key.user_id.as_str())
            .fetch_one(&mut *tx)
            .await?;
        let role =
            WorkspaceRole::parse(&role_token).ok_or(HumanEnrollFailure::CorruptRole(role_token))?;

        tx.commit().await?;

        Ok(HumanEnrollmentCommit {
            member: WorkspaceUserMembership {
                workspace_id: key.workspace_id,
                user_id: key.user_id,
                joined_at_ms: stored_joined_at,
                role,
            },
            created,
        })
    }
}

impl WorkspaceAccessPolicy for SqliteAccessStore {
    async fn authorize_member_governance(
        &self,
        principal: &Principal,
        workspace_id: &WorkspaceId,
    ) -> Result<WorkspaceGovernance, PersistenceFailure> {
        let facts = self
            .governance_facts(principal, workspace_id)
            .await
            .map_err(PersistenceFailure::from_source)?;
        Ok(governance_ceiling(principal, facts))
    }
}

impl WorkspaceGovernanceFactsRead for SqliteAccessStore {
    async fn read_governance_facts(
        &self,
        principal: &Principal,
        workspace_id: &WorkspaceId,
    ) -> Result<WorkspaceGovernanceFacts, PersistenceFailure> {
        // The raw facts, off the SAME read the ceiling door consumes. The floor
        // rule (`governance_floor_verdict`) is applied by the metadata surfaces
        // with owner-fidelity; this adapter derives nothing.
        self.governance_facts(principal, workspace_id)
            .await
            .map_err(PersistenceFailure::from_source)
    }
}

impl PeerTargetMembership for SqliteAccessStore {
    /// The peer-profile target's membership edge, reusing this store's pool and
    /// `workspace_members` authority — no second membership model. Keyed on the
    /// exact `(workspace_id, agent_id)` pair, regular edge only: an agent's home
    /// workspace carries no such row, so it is `false` here even where the
    /// caller's horizon can see the home.
    async fn agent_is_member(
        &self,
        workspace: &WorkspaceId,
        agent: &AgentId,
    ) -> Result<bool, PersistenceFailure> {
        let present: i64 = sqlx::query_scalar(SELECT_AGENT_IS_WORKSPACE_MEMBER)
            .bind(workspace.as_str())
            .bind(agent.as_str())
            .fetch_one(&self.pool)
            .await
            .map_err(PersistenceFailure::from_source)?;
        Ok(present != 0)
    }
}

impl AgentEnrollmentReadPort for SqliteAccessStore {
    async fn agent(
        &self,
        agent_id: &AgentId,
    ) -> Result<Option<AgentAuthorityFacts>, PersistenceFailure> {
        self.agent_authority(agent_id)
            .await
            .map_err(PersistenceFailure::from_source)
    }
}

impl AgentEnrollmentUnitOfWork for SqliteAccessStore {
    async fn enroll_agent(
        &self,
        key: EnrollmentKey,
        seed_on_create: &[Capability],
        joined_at_candidate: i64,
    ) -> Result<AgentWorkspaceMembership, PersistenceFailure> {
        // The driver error is handed over as the source; the refusal is not
        // logged here — the boundary that renders it owns that record.
        self.enroll_agent_transaction(key, seed_on_create, joined_at_candidate)
            .await
            .map_err(PersistenceFailure::from_source)
    }
}

impl HumanEnrollmentReadPort for SqliteAccessStore {
    async fn user(&self, user_id: &UserId) -> Result<Option<HumanUser>, PersistenceFailure> {
        self.user_record(user_id)
            .await
            .map_err(PersistenceFailure::from_source)
    }
}

impl HumanEnrollmentUnitOfWork for SqliteAccessStore {
    async fn enroll_human(
        &self,
        key: HumanEnrollmentKey,
        actor: Principal,
        joined_at_candidate: i64,
    ) -> Result<HumanEnrollmentCommit, AddHumanMemberError> {
        // The in-lock authority refusal is a typed `Forbidden`; every driver
        // fault (and the unreachable corrupt-role case) is handed over as the
        // source of a `PersistenceFailure` and rendered as a safe `500`. Nothing
        // is logged here — the boundary that renders it owns that record.
        self.enroll_human_transaction(key, actor, joined_at_candidate)
            .await
            .map_err(|failure| match failure {
                HumanEnrollFailure::Forbidden => AddHumanMemberError::Forbidden,
                HumanEnrollFailure::Sql(error) => {
                    AddHumanMemberError::Persistence(PersistenceFailure::from_source(error))
                }
                HumanEnrollFailure::CorruptRole(token) => AddHumanMemberError::Persistence(
                    PersistenceFailure::from_source(CorruptRoleToken(token)),
                ),
            })
    }
}
