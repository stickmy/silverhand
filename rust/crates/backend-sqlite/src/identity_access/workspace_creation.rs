//! The durable workspace-create writes: the workspace row + owner/user members,
//! and the Onboarding channel + its members + per-agent system messages.
//!
//! The durable workspace-create writes: `provision_workspace` runs the WHOLE
//! synchronous durable-setup phase — name verdict, per-agent existence/ownership
//! replay, workspace + owner/user members, per-agent membership/capabilities, and the
//! Onboarding channel + founding roster — in ONE `BEGIN IMMEDIATE` transaction so no
//! competing writer can interleave; ids are minted + the clock
//! sampled inside, only after the gates pass. `append_agent_onboarding_message` is a
//! SEPARATE per-agent step the route interleaves with deliver, each committing its own
//! message.

use silverhand_backend_core::{
    foundation::identity::{AgentId, UserId, WorkspaceId},
    foundation::persistence::PersistenceFailure,
    identity_access::workspace_creation::{
        AgentOnboardingIntent, CommittedOnboardingMessage, CreatedWorkspaceFace,
        FirstOnboardingOutcome, OnboardingChannelCommitted, ProvisionedWorkspace,
        WorkspaceCreationWrite, WorkspaceProvisionOutcome, WorkspaceProvisionRequest,
    },
};
use sqlx::Row;
use thiserror::Error;

use crate::conversation_messaging::messaging::MessagingStore;

/// The durable create-write authority — a shared-pool clone of the ONE messaging
/// store pool (the single writer). Opens no second connection authority.
#[derive(Clone)]
pub struct SqliteWorkspaceCreation {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    /// The durable workspace-create write authority, sharing this store's pool.
    pub fn workspace_creation(&self) -> SqliteWorkspaceCreation {
        SqliteWorkspaceCreation {
            pool: self.pool().clone(),
        }
    }
}

impl SqliteWorkspaceCreation {
    /// Direct pool constructor for composition tests / integration harnesses.
    pub fn from_pool(pool: impl Into<crate::foundation::database::SqliteDatabase>) -> Self {
        let pool = pool.into();
        Self { pool }
    }
}

#[derive(Debug, Error)]
pub enum WorkspaceCreationStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("committed workspace row disappeared immediately after insert")]
    CommittedWorkspaceMissing,
    #[error("stored workspace identity is malformed")]
    MalformedIdentity,
    #[error("onboarding message write failed its shape seal: {0}")]
    OnboardingWriteMismatch(&'static str),
    #[error("stored workspace numeric is outside the JS-safe integer domain")]
    CorruptNumber,
    #[error("the environment could not mint an id for the provisioning write")]
    Environment(
        #[from]
        silverhand_backend_core::conversation_messaging::messaging::environment::EnvironmentUnavailable,
    ),
    #[error("onboarding message append failed")]
    Persistence(PersistenceFailure),
}

/// JS-safe integer gate at store->projection (#48): a stored timestamp JS can't
/// represent exactly fails closed rather than truncating on the wire.
fn wsc_js_safe(value: i64) -> Result<i64, WorkspaceCreationStoreError> {
    silverhand_backend_core::foundation::wire_number::JsSafeInteger::new(value)
        .map(|v| v.get())
        .map_err(|_| WorkspaceCreationStoreError::CorruptNumber)
}

impl From<WorkspaceCreationStoreError> for PersistenceFailure {
    fn from(value: WorkspaceCreationStoreError) -> Self {
        Self::from_source(value)
    }
}

const ROOT_WRITE_BEGIN: &str = "BEGIN IMMEDIATE";
impl WorkspaceCreationWrite for SqliteWorkspaceCreation {
    /// The whole synchronous durable-setup phase in ONE `BEGIN IMMEDIATE`
    /// transaction: name verdict → per-agent
    /// existence+ownership replay → workspace+owner+users insert → per-agent
    /// membership+capability seed → onboarding channel+roster. No competing writer
    /// interleaves any step, so a racing delete/ownership-change lands wholly before
    /// or wholly after this boundary. Every refusal rolls the whole tx back (zero
    /// reach). The FIRST accepted agent's onboarding MESSAGE is also written here, in
    /// this same tx under a savepoint — see `first_onboarding`; only
    /// the SUBSEQUENT agents' messages stay a separate route-interleaved
    /// append→deliver step.
    async fn provision_workspace<E, C>(
        &self,
        request: &WorkspaceProvisionRequest<'_>,
        entropy: &E,
        clock: &C,
    ) -> Result<WorkspaceProvisionOutcome, PersistenceFailure>
    where
        E: silverhand_backend_core::conversation_messaging::messaging::environment::SendEntropy
            + Sync,
        C: silverhand_backend_core::identity_access::workspace_creation::WorkspaceProvisionClock,
    {
        use silverhand_backend_core::conversation_messaging::messaging::{
            mint_conversation_id, mint_workspace_id,
        };
        use silverhand_backend_core::identity_access::access::DEFAULT_AGENT_CAPABILITIES;
        use silverhand_backend_core::identity_access::workspace_creation::AcceptedMemberAgent;

        let mut tx = self
            .pool
            .begin_with(ROOT_WRITE_BEGIN)
            .await
            .map_err(WorkspaceCreationStoreError::from)?;

        // (1) Name verdict UNDER the lock (#11 name-before-agent). Taken -> NameTaken,
        // whole tx rolls back (drop), zero writes. This is decided BEFORE the clock is
        // sampled or any id is minted (#9/#31), so a failing entropy/clock can never
        // pre-empt this product refusal.
        let name_taken: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM workspaces WHERE name_key = ? AND owner_agent_id IS NULL)",
        )
        .bind(request.name.key())
        .fetch_one(&mut *tx)
        .await
        .map_err(WorkspaceCreationStoreError::from)?;
        if name_taken {
            drop(tx);
            tracing::warn!(
                workspace_address_event = "conflict",
                workspace_id = Option::<&str>::None,
                key_hash = hash_workspace_address_key(request.name.key()),
                "workspace address conflict"
            );
            return Ok(WorkspaceProvisionOutcome::NameTaken);
        }

        // (2) Per requested agent, IN the lock: existence + owner replay (#28), AND
        // capture the committed handle/runtime post-image from the SAME lock (no
        // post-commit live reread, no missing fallback). Unknown -> silently dropped;
        // known-unowned -> AgentNotOwned (whole tx rolls back). The accepted witnesses
        // keep request order + multiplicity (#10) for the onboarding fan-out. Still
        // BEFORE clock/mint, so a failing entropy/clock cannot pre-empt this refusal.
        let mut accepted: Vec<AcceptedMemberAgent> = Vec::new();
        let mut not_owned: Vec<AgentId> = Vec::new();
        for agent in request.requested_agents {
            let row =
                sqlx::query("SELECT created_by_user_id, name, runtime FROM agents WHERE id = ?")
                    .bind(agent.as_str())
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(WorkspaceCreationStoreError::from)?;
            match row {
                // Unknown agent — silently dropped (no rule the caller could act on).
                None => {}
                Some(row) => {
                    let created_by: String = row
                        .try_get("created_by_user_id")
                        .map_err(WorkspaceCreationStoreError::from)?;
                    if created_by == request.owner_user.as_str() {
                        let handle_raw: String = row
                            .try_get("name")
                            .map_err(WorkspaceCreationStoreError::from)?;
                        let runtime: String = row
                            .try_get("runtime")
                            .map_err(WorkspaceCreationStoreError::from)?;
                        // Parse the handle into a CLOSED typed ParticipantHandle IN the
                        // lock: a malformed stored handle for ANY
                        // accepted member is a typed fault that rolls the WHOLE tx back
                        // here — BEFORE any onboarding message/deliver — never a partial
                        // fan-out that drops a later malformed member silently.
                        let handle = silverhand_backend_core::conversation_messaging::messaging::ParticipantHandle::parse(handle_raw)
                            .map_err(|_| WorkspaceCreationStoreError::MalformedIdentity)?;
                        accepted.push(AcceptedMemberAgent {
                            agent: agent.clone(),
                            handle,
                            runtime,
                        });
                    } else {
                        not_owned.push(agent.clone());
                    }
                }
            }
        }
        if !not_owned.is_empty() {
            drop(tx);
            return Ok(WorkspaceProvisionOutcome::AgentNotOwned {
                agent_ids: not_owned,
            });
        }

        // Both product gates passed — NOW sample the clock and mint the ids (#9/#31).
        // A failing clock/entropy from here is a genuine environment fault (500), never
        // a mislabelled product refusal, because both refusals already returned above.
        let now_ms = clock.now_ms();
        let workspace_id =
            mint_workspace_id(entropy).map_err(WorkspaceCreationStoreError::Environment)?;
        let onboarding_channel_id =
            mint_conversation_id(entropy).map_err(WorkspaceCreationStoreError::Environment)?;

        // (3) Workspace row + owner pointer + owner membership + validated user members.
        let insert = sqlx::query(
            "INSERT INTO workspaces (id, name, name_key, description, owner_agent_id, owner_user_id, archived_at, created_at, updated_at) \
             VALUES (?, ?, ?, ?, NULL, ?, NULL, ?, ?)",
        )
        .bind(workspace_id.as_str())
        .bind(request.name.name())
        .bind(request.name.key())
        .bind(request.description)
        .bind(request.owner_user.as_str())
        .bind(now_ms)
        .bind(now_ms)
        .execute(&mut *tx)
        .await;
        if let Err(error) = insert {
            // The in-lock name pre-check already resolved name conflicts; a failure here
            // is a genuine persistence fault (e.g. minted-id collision), NOT a name
            // conflict — propagate it, never masquerade it as NameTaken.
            drop(tx);
            return Err(WorkspaceCreationStoreError::from(error).into());
        }
        sqlx::query(
            "INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) VALUES (?, ?, ?, 'owner')",
        )
        .bind(workspace_id.as_str())
        .bind(request.owner_user.as_str())
        .bind(now_ms)
        .execute(&mut *tx)
        .await
        .map_err(WorkspaceCreationStoreError::from)?;

        let mut member_users = vec![request.owner_user.clone()];
        for member in request.member_user_ids {
            if member == &request.owner_user {
                continue;
            }
            let inserted = sqlx::query(
                "INSERT OR IGNORE INTO workspace_users (workspace_id, user_id, joined_at, role) \
                 SELECT ?, ?, ?, 'member' WHERE EXISTS (SELECT 1 FROM users WHERE id = ?)",
            )
            .bind(workspace_id.as_str())
            .bind(member.as_str())
            .bind(now_ms)
            .bind(member.as_str())
            .execute(&mut *tx)
            .await
            .map_err(WorkspaceCreationStoreError::from)?;
            if inserted.rows_affected() > 0 {
                member_users.push(member.clone());
            }
        }

        // (4) Per accepted agent, IN the SAME lock: membership + default capability
        // seed (the same edges the reused enrollment authority writes; existence +
        // ownership already replayed above, so no FK-on-stale-authority is possible).
        // Dedup for the idempotent membership INSERTs.
        let mut enrolled_once: Vec<&AgentId> = Vec::new();
        for accepted_agent in &accepted {
            let agent = &accepted_agent.agent;
            if enrolled_once.contains(&agent) {
                continue;
            }
            enrolled_once.push(agent);
            sqlx::query(
                "INSERT OR IGNORE INTO workspace_members (workspace_id, agent_id, joined_at) VALUES (?, ?, ?)",
            )
            .bind(workspace_id.as_str())
            .bind(agent.as_str())
            .bind(now_ms)
            .execute(&mut *tx)
            .await
            .map_err(WorkspaceCreationStoreError::from)?;
            for capability in DEFAULT_AGENT_CAPABILITIES {
                sqlx::query(
                    "INSERT OR IGNORE INTO workspace_agent_capabilities (workspace_id, agent_id, capability) VALUES (?, ?, ?)",
                )
                .bind(workspace_id.as_str())
                .bind(agent.as_str())
                .bind(capability.as_str())
                .execute(&mut *tx)
                .await
                .map_err(WorkspaceCreationStoreError::from)?;
            }
        }

        // (5) Onboarding channel + founding roster (all current workspace users + each
        // member agent), same tx.
        sqlx::query(
            "INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, title_key, status, visibility, created_at, updated_at) \
             VALUES (?, ?, 'workspace', 'open', 'Onboarding', 'onboarding', 'idle', 'private', ?, ?)",
        )
        .bind(onboarding_channel_id.as_str())
        .bind(workspace_id.as_str())
        .bind(now_ms)
        .bind(now_ms)
        .execute(&mut *tx)
        .await
        .map_err(WorkspaceCreationStoreError::from)?;
        let user_rows = sqlx::query("SELECT user_id FROM workspace_users WHERE workspace_id = ?")
            .bind(workspace_id.as_str())
            .fetch_all(&mut *tx)
            .await
            .map_err(WorkspaceCreationStoreError::from)?;
        for row in &user_rows {
            let user_id: String = row
                .try_get("user_id")
                .map_err(WorkspaceCreationStoreError::from)?;
            sqlx::query(
                "INSERT OR IGNORE INTO conversation_users (conversation_id, user_id, joined_at) VALUES (?, ?, ?)",
            )
            .bind(onboarding_channel_id.as_str())
            .bind(&user_id)
            .bind(now_ms)
            .execute(&mut *tx)
            .await
            .map_err(WorkspaceCreationStoreError::from)?;
        }
        for agent in &enrolled_once {
            sqlx::query(
                "INSERT OR IGNORE INTO conversation_agents (conversation_id, agent_id, joined_at) VALUES (?, ?, ?)",
            )
            .bind(onboarding_channel_id.as_str())
            .bind(agent.as_str())
            .bind(now_ms)
            .execute(&mut *tx)
            .await
            .map_err(WorkspaceCreationStoreError::from)?;
        }

        // The FIRST accepted agent's onboarding message is made
        // durable INSIDE THIS SAME transaction (the TS synchronous durable prefix has no
        // await point between the workspace setup and the first accepted message being
        // durable, and no other writer can interleave — it is all under this one
        // `BEGIN IMMEDIATE` lock). But the message attempt is wrapped in a SAVEPOINT so
        // its failure preserves the setup residue: on any first-message fault (entropy
        // mint / append / seal) we ROLLBACK TO the savepoint (message + stream head carry
        // NO half-write) and still COMMIT the workspace/members/private-channel setup,
        // returning `FirstOnboardingOutcome::Failed` (the route maps it to a 500 with ZERO
        // deliver/start effect). The clock sample is infallible; entropy is minted inside
        // the savepoint scope so its fault also preserves setup.
        let first_onboarding = if let Some(first) = accepted.first() {
            use silverhand_backend_core::conversation_messaging::messaging::agent_onboarding::WorkspaceOnboardingPeer;
            let others: Vec<WorkspaceOnboardingPeer> = accepted
                .iter()
                .filter(|member| member.agent != first.agent)
                .map(|member| WorkspaceOnboardingPeer {
                    handle: member.handle.clone(),
                    runtime: member.runtime.clone(),
                })
                .collect();

            sqlx::query("SAVEPOINT first_onboarding")
                .execute(&mut *tx)
                .await
                .map_err(WorkspaceCreationStoreError::from)?;

            // The whole first-message attempt as a fallible unit; the clock is sampled
            // fresh (distinct instant from the workspace stamp, R3-next #4).
            let attempt = append_first_onboarding_in_savepoint(
                &mut tx,
                &onboarding_channel_id,
                request.name.name(),
                &first.handle,
                &others,
                entropy,
                clock,
            )
            .await;

            match attempt {
                Ok(message_id) => {
                    sqlx::query("RELEASE SAVEPOINT first_onboarding")
                        .execute(&mut *tx)
                        .await
                        .map_err(WorkspaceCreationStoreError::from)?;
                    FirstOnboardingOutcome::Committed(CommittedOnboardingMessage {
                        agent: first.agent.clone(),
                        message_id,
                    })
                }
                Err(_fault) => {
                    // Discard ONLY the message rows/head bump; keep the setup residue.
                    sqlx::query("ROLLBACK TO SAVEPOINT first_onboarding")
                        .execute(&mut *tx)
                        .await
                        .map_err(WorkspaceCreationStoreError::from)?;
                    sqlx::query("RELEASE SAVEPOINT first_onboarding")
                        .execute(&mut *tx)
                        .await
                        .map_err(WorkspaceCreationStoreError::from)?;
                    FirstOnboardingOutcome::Failed
                }
            }
        } else {
            FirstOnboardingOutcome::NoAgents
        };

        // Read the committed face back in the SAME tx (JS-safe gate on its stamps).
        let row = sqlx::query(
            "SELECT id, name, description, owner_user_id, created_at, updated_at \
             FROM workspaces WHERE id = ?",
        )
        .bind(workspace_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(WorkspaceCreationStoreError::from)?
        .ok_or(WorkspaceCreationStoreError::CommittedWorkspaceMissing)?;
        let face = decode_created_face(&row)?;

        // Read the committed onboarding-channel facts back in the SAME tx:
        // the 201 body reflects the STORED visibility/title/status (an
        // onboarding conversation is `private`) rather than a hard-coded literal that
        // could drift from the insert.
        let channel_row =
            sqlx::query("SELECT visibility, title, status FROM conversations WHERE id = ?")
                .bind(onboarding_channel_id.as_str())
                .fetch_optional(&mut *tx)
                .await
                .map_err(WorkspaceCreationStoreError::from)?
                .ok_or(WorkspaceCreationStoreError::CommittedWorkspaceMissing)?;
        let channel_visibility: String = channel_row
            .try_get("visibility")
            .map_err(WorkspaceCreationStoreError::from)?;
        let channel_title: String = channel_row
            .try_get("title")
            .map_err(WorkspaceCreationStoreError::from)?;
        let channel_status: String = channel_row
            .try_get("status")
            .map_err(WorkspaceCreationStoreError::from)?;

        tx.commit()
            .await
            .map_err(WorkspaceCreationStoreError::from)?;
        tracing::warn!(
            workspace_address_event = "assignment",
            action = "create",
            workspace_id = face.id.as_str(),
            old_key_hash = Option::<&str>::None,
            new_key_hash = hash_workspace_address_key(request.name.key()),
            "workspace address assigned"
        );
        Ok(WorkspaceProvisionOutcome::Provisioned(Box::new(
            ProvisionedWorkspace {
                member_users,
                face,
                onboarding_channel: OnboardingChannelCommitted {
                    channel: onboarding_channel_id,
                    workspace: workspace_id,
                    visibility: channel_visibility,
                    title: channel_title,
                    status: channel_status,
                    created_at: now_ms,
                    updated_at: now_ms,
                },
                // Accepted agents with committed handle/runtime post-image (#28) and
                // request-order multiplicity preserved (#10).
                member_agents: accepted,
                // The FIRST accepted agent's onboarding message, made durable in THIS
                // same tx above (#R5-3); `None` iff no agent was accepted.
                first_onboarding,
            },
        )))
    }

    /// Append ONE agent's onboarding system message through the canonical primitive
    /// (reserve seq → 14-column system insert → main-line stream-head bump) in its own
    /// committed transaction, so the channel's `latest_message_seq`/head advance
    /// exactly as a normal send's and the next real send takes N+1. Called per agent
    /// AFTER the prior agent's deliver succeeded, so a deliver failure stops the loop
    /// before this agent's row is ever written.
    async fn append_agent_onboarding_message<E, C>(
        &self,
        intent: &AgentOnboardingIntent,
        entropy: &E,
        clock: &C,
    ) -> Result<CommittedOnboardingMessage, PersistenceFailure>
    where
        E: silverhand_backend_core::conversation_messaging::messaging::environment::SendEntropy
            + Sync,
        C: silverhand_backend_core::identity_access::workspace_creation::WorkspaceProvisionClock,
    {
        let mut tx = self
            .pool
            .begin_with(ROOT_WRITE_BEGIN)
            .await
            .map_err(WorkspaceCreationStoreError::from)?;
        // The intent carries NO id — the writer mints it (and samples the
        // clock) itself in-lock, so the route does zero entropy. Render + append + seal
        // via the shared helper; a fault aborts THIS tx (no setup residue to protect on
        // the subsequent-agent path — each is its own tx).
        let message_id = render_and_append_onboarding(
            &mut tx,
            &intent.conversation,
            &intent.workspace_name,
            &intent.recipient_handle,
            &intent.others,
            entropy,
            clock,
        )
        .await?;
        tx.commit()
            .await
            .map_err(WorkspaceCreationStoreError::from)?;
        Ok(CommittedOnboardingMessage {
            agent: intent.agent.clone(),
            message_id,
        })
    }
}

/// Render + mint + sample + append + seal ONE onboarding message on the given writer
/// connection. The id is minted from `entropy` and the timestamp
/// sampled from `clock` HERE (in-lock), never route-side; the body is rendered from the
/// recipient handle + peers. Returns the minted `MessageId` on success. Any fault
/// (entropy mint / append / seal) returns `Err` WITHOUT committing — the caller decides
/// whether that aborts the whole tx (subsequent-agent path) or only a savepoint
/// (first-message path).
async fn render_and_append_onboarding<E, C>(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    conversation: &silverhand_backend_core::foundation::identity::ConversationId,
    workspace_name: &str,
    recipient_handle: &silverhand_backend_core::conversation_messaging::messaging::ParticipantHandle,
    others: &[silverhand_backend_core::conversation_messaging::messaging::agent_onboarding::WorkspaceOnboardingPeer],
    entropy: &E,
    clock: &C,
) -> Result<
    silverhand_backend_core::conversation_messaging::messaging::MessageId,
    WorkspaceCreationStoreError,
>
where
    E: silverhand_backend_core::conversation_messaging::messaging::environment::SendEntropy + Sync,
    C: silverhand_backend_core::identity_access::workspace_creation::WorkspaceProvisionClock,
{
    use crate::conversation_messaging::messaging::append_onboarding_system_message_on;
    use silverhand_backend_core::conversation_messaging::messaging::MessageId;
    use silverhand_backend_core::conversation_messaging::messaging::agent_onboarding::OnboardingSystemMessage;

    let message_id = MessageId::mint(entropy).map_err(WorkspaceCreationStoreError::Environment)?;
    let now_ms = clock.now_ms();
    let message = OnboardingSystemMessage::workspace_onboarding(
        conversation.clone(),
        message_id.clone(),
        workspace_name,
        recipient_handle,
        others,
        now_ms,
    );
    let witness = append_onboarding_system_message_on(&mut *tx, &message)
        .await
        .map_err(WorkspaceCreationStoreError::Persistence)?;
    message.verify_written(&witness).map_err(|mismatch| {
        WorkspaceCreationStoreError::OnboardingWriteMismatch(mismatch.reason())
    })?;
    Ok(message_id)
}

/// The first-message attempt inside the provision savepoint: a thin
/// wrapper over [`render_and_append_onboarding`] so the caller can roll back ONLY the
/// savepoint on any fault while committing the setup residue.
async fn append_first_onboarding_in_savepoint<E, C>(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    conversation: &silverhand_backend_core::foundation::identity::ConversationId,
    workspace_name: &str,
    recipient_handle: &silverhand_backend_core::conversation_messaging::messaging::ParticipantHandle,
    others: &[silverhand_backend_core::conversation_messaging::messaging::agent_onboarding::WorkspaceOnboardingPeer],
    entropy: &E,
    clock: &C,
) -> Result<
    silverhand_backend_core::conversation_messaging::messaging::MessageId,
    WorkspaceCreationStoreError,
>
where
    E: silverhand_backend_core::conversation_messaging::messaging::environment::SendEntropy + Sync,
    C: silverhand_backend_core::identity_access::workspace_creation::WorkspaceProvisionClock,
{
    render_and_append_onboarding(
        tx,
        conversation,
        workspace_name,
        recipient_handle,
        others,
        entropy,
        clock,
    )
    .await
}

/// The dogfood address-key correlation token — `sha256:<first 24 hex>`, never the
/// display name. Mirrors the reference `hashWorkspaceAddressKey` so the dogfood
/// JSONL window correlates across the TS and Rust producers.
fn hash_workspace_address_key(key: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(key.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256:{}", &hex[..24])
}

fn decode_created_face(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<CreatedWorkspaceFace, WorkspaceCreationStoreError> {
    let id: String = row.try_get("id")?;
    let id = WorkspaceId::parse(id).map_err(|_| WorkspaceCreationStoreError::MalformedIdentity)?;
    let owner_user_id: Option<String> = row.try_get("owner_user_id")?;
    let owner_user_id = match owner_user_id {
        None => None,
        Some(raw) => {
            Some(UserId::parse(raw).map_err(|_| WorkspaceCreationStoreError::MalformedIdentity)?)
        }
    };
    Ok(CreatedWorkspaceFace {
        id,
        name: row.try_get("name")?,
        description: row.try_get("description")?,
        owner_user_id,
        created_at: wsc_js_safe(row.try_get("created_at")?)?,
        updated_at: wsc_js_safe(row.try_get("updated_at")?)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use silverhand_backend_core::conversation_messaging::messaging::ParticipantHandle;
    use silverhand_backend_core::conversation_messaging::messaging::agent_onboarding::WorkspaceOnboardingPeer;
    use silverhand_backend_core::identity_access::workspace_name::CanonicalWorkspaceName;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use sqlx::{Connection, Executor, SqliteConnection};

    const CURRENT_SCHEMA: &str =
        include_str!("../../../../../rust/test-fixtures/migration-baseline/schema-current.sql");

    const SEED_SQL: &str = "\
INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES ('user-1','owner','Owner',1,1); \
INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES ('user-2','member','Member',1,1); \
INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1','Host','local',1); \
INSERT INTO agents (id, name, display_name, runtime, host_id, created_by_user_id, created_at, updated_at, session_scope) \
  VALUES ('agent-a','alice','Alice','claude','host-1','user-1',1,1,'conversation'); \
INSERT INTO agents (id, name, display_name, runtime, host_id, created_by_user_id, created_at, updated_at, session_scope) \
  VALUES ('agent-b','bob','Bob','codex','host-1','user-1',1,1,'conversation');";

    async fn open_seeded() -> (tempfile::TempDir, SqliteWorkspaceCreation, sqlx::SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspace-creation.sqlite");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        db.execute(sqlx::raw_sql(CURRENT_SCHEMA)).await.unwrap();
        db.execute(sqlx::raw_sql(SEED_SQL)).await.unwrap();
        db.close().await.unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        (dir, SqliteWorkspaceCreation::from_pool(pool.clone()), pool)
    }

    fn name(raw: &str) -> CanonicalWorkspaceName {
        CanonicalWorkspaceName::parse(raw).unwrap()
    }
    fn user(id: &str) -> UserId {
        UserId::parse(id).unwrap()
    }
    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).unwrap()
    }

    /// Build a provision request over borrowed name/users/agents. The writer mints
    /// the ids and samples the clock itself, so the request carries neither.
    fn req<'a>(
        nm: &'a CanonicalWorkspaceName,
        owner: UserId,
        users: &'a [UserId],
        agents: &'a [AgentId],
    ) -> WorkspaceProvisionRequest<'a> {
        WorkspaceProvisionRequest {
            name: nm,
            description: None,
            owner_user: owner,
            member_user_ids: users,
            requested_agents: agents,
        }
    }

    /// A fixed provisioning clock the writer samples after its gates.
    struct TestClock(i64);
    impl silverhand_backend_core::identity_access::workspace_creation::WorkspaceProvisionClock
        for TestClock
    {
        fn now_ms(&self) -> i64 {
            self.0
        }
    }

    /// Entropy that always fails — proves a failing mint AFTER the gates is an Err
    /// (env fault), and can NEVER pre-empt a product refusal decided before it.
    struct FailingEntropy;
    impl silverhand_backend_core::conversation_messaging::messaging::environment::SendEntropy
        for FailingEntropy
    {
        fn fill(
            &self,
            _buffer: &mut [u8],
        ) -> Result<
            (),
            silverhand_backend_core::conversation_messaging::messaging::environment::EnvironmentUnavailable,
        >{
            Err(
                silverhand_backend_core::conversation_messaging::messaging::environment::EnvironmentUnavailable::from_source(
                    std::io::Error::other("entropy down"),
                ),
            )
        }
    }

    /// Entropy whose Nth `fill` call fails (1-indexed) and all others succeed with the
    /// real host entropy — to isolate the FIRST-MESSAGE entropy arm: the
    /// provision mints workspace-id (1st fill) + onboarding-channel-id (2nd), then the
    /// first message id (3rd), so failing on the 3rd fails ONLY the first message and
    /// exercises the savepoint rollback while the setup residue commits.
    struct NthMintFailsEntropy {
        n: std::sync::atomic::AtomicUsize,
        fail_at: usize,
    }
    impl NthMintFailsEntropy {
        fn at(fail_at: usize) -> Self {
            Self {
                n: std::sync::atomic::AtomicUsize::new(0),
                fail_at,
            }
        }
    }
    impl silverhand_backend_core::conversation_messaging::messaging::environment::SendEntropy
        for NthMintFailsEntropy
    {
        fn fill(
            &self,
            buffer: &mut [u8],
        ) -> Result<
            (),
            silverhand_backend_core::conversation_messaging::messaging::environment::EnvironmentUnavailable,
        >{
            let call = self.n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if call == self.fail_at {
                return Err(
                    silverhand_backend_core::conversation_messaging::messaging::environment::EnvironmentUnavailable::from_source(
                        std::io::Error::other("nth entropy down"),
                    ),
                );
            }
            crate::conversation_messaging::messaging::HostEnvironment.fill(buffer)
        }
    }

    /// Provision with the real host entropy + a fixed clock at `now`.
    async fn provision(
        writer: &SqliteWorkspaceCreation,
        request: &WorkspaceProvisionRequest<'_>,
        now: i64,
    ) -> Result<
        WorkspaceProvisionOutcome,
        silverhand_backend_core::foundation::persistence::PersistenceFailure,
    > {
        writer
            .provision_workspace(
                request,
                &crate::conversation_messaging::messaging::HostEnvironment,
                &TestClock(now),
            )
            .await
    }

    #[tokio::test]
    async fn a_js_unsafe_stored_timestamp_fails_the_created_face_closed_not_truncated() {
        // #48: the created-workspace face is echoed on the wire where JS reads
        // created_at/updated_at as f64. A stored timestamp beyond MAX_SAFE_INTEGER
        // cannot round-trip exactly, so the store->projection decode must fail
        // closed (CorruptNumber) rather than hand back a silently truncated number.
        let (_dir, writer, _pool) = open_seeded().await;
        let js_unsafe = 9_007_199_254_740_993_i64; // MAX_SAFE_INTEGER + 2
        let nm = name("Unsafe");
        let outcome = provision(&writer, &req(&nm, user("user-1"), &[], &[]), js_unsafe).await;
        assert!(
            outcome.is_err(),
            "a JS-unsafe stored timestamp must fail the face decode closed, never be truncated onto the wire: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn failing_entropy_after_gates_is_an_env_fault_never_a_masked_product_refusal() {
        // #31/#9: the writer mints ids + samples the clock ONLY after its name +
        // agent-owner gates pass, so a failing entropy on a free name × owned agents is
        // a genuine env fault (Err) with zero rows — not a mislabelled refusal.
        let (_dir, writer, pool) = open_seeded().await;
        let team = name("Team");
        let agents = [agent("agent-a")];
        let outcome = writer
            .provision_workspace(
                &req(&team, user("user-1"), &[], &agents),
                &FailingEntropy,
                &TestClock(10),
            )
            .await;
        assert!(
            outcome.is_err(),
            "failing mint is an env fault: {outcome:?}"
        );
        let ws_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM workspaces")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(ws_rows, 0, "a failed mint wrote nothing");
    }

    #[tokio::test]
    async fn failing_entropy_never_pre_empts_a_name_taken_refusal() {
        // #31 falsifier: failing entropy × a taken name must STILL be NameTaken (409),
        // decided before the mint is ever attempted — never a 500.
        let (_dir, writer, _pool) = open_seeded().await;
        let team = name("Team");
        provision(&writer, &req(&team, user("user-1"), &[], &[]), 10)
            .await
            .unwrap();
        let team_again = name("team");
        let outcome = writer
            .provision_workspace(
                &req(&team_again, user("user-1"), &[], &[]),
                &FailingEntropy,
                &TestClock(11),
            )
            .await
            .expect("name-taken is decided before the mint, so no env error surfaces");
        assert_eq!(outcome, WorkspaceProvisionOutcome::NameTaken);
    }

    #[tokio::test]
    async fn failing_entropy_never_pre_empts_an_unowned_agent_refusal() {
        // #31 falsifier: failing entropy × an unowned agent must STILL be AgentNotOwned
        // (403), decided before the mint — never a 500.
        let (_dir, writer, pool) = open_seeded().await;
        sqlx::query("UPDATE agents SET created_by_user_id='user-2' WHERE id='agent-b'")
            .execute(&pool)
            .await
            .unwrap();
        let team = name("Team");
        let agents = [agent("agent-b")];
        let outcome = writer
            .provision_workspace(
                &req(&team, user("user-1"), &[], &agents),
                &FailingEntropy,
                &TestClock(10),
            )
            .await
            .expect("unowned is decided before the mint, so no env error surfaces");
        assert_eq!(
            outcome,
            WorkspaceProvisionOutcome::AgentNotOwned {
                agent_ids: vec![agent("agent-b")]
            }
        );
    }

    #[tokio::test]
    async fn accepted_agent_witness_carries_stored_handle_and_runtime() {
        // The returned witness carries the admitted agent's stored identity.
        let (_dir, writer, _pool) = open_seeded().await;
        let team = name("Team");
        let agents = [agent("agent-a")];
        let WorkspaceProvisionOutcome::Provisioned(provisioned) =
            provision(&writer, &req(&team, user("user-1"), &[], &agents), 10)
                .await
                .unwrap()
        else {
            panic!("expected Provisioned");
        };
        assert_eq!(provisioned.member_agents.len(), 1);
        assert_eq!(provisioned.member_agents[0].handle.as_str(), "alice");
        assert_eq!(provisioned.member_agents[0].runtime, "claude");
    }

    #[tokio::test]
    async fn a_malformed_second_member_handle_fails_the_whole_provision_closed() {
        // A LATER accepted member with a stored handle that is not a
        // valid ParticipantHandle must fail the WHOLE provisioning tx closed BEFORE any
        // durable write — never a partial state where the first member is committed and
        // the malformed one is silently dropped. (The onboarding fan-out is downstream
        // of provisioning; a zero-write provision means zero message/zero deliver.)
        let (_dir, writer, pool) = open_seeded().await;
        // agent-b's stored name has a space — not a valid handle grammar.
        sqlx::query("UPDATE agents SET name='bad name' WHERE id='agent-b'")
            .execute(&pool)
            .await
            .unwrap();
        let team = name("Team");
        let agents = [agent("agent-a"), agent("agent-b")];
        let outcome = provision(&writer, &req(&team, user("user-1"), &[], &agents), 10).await;
        assert!(
            outcome.is_err(),
            "a malformed member handle must fail the whole provision closed: {outcome:?}"
        );
        // Zero reach: no workspace, no channel, no membership for the first member either.
        let ws_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM workspaces")
            .fetch_one(&pool)
            .await
            .unwrap();
        let member_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM workspace_members")
            .fetch_one(&pool)
            .await
            .unwrap();
        let chan_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            (ws_rows, member_rows, chan_rows),
            (0, 0, 0),
            "no partial durable residue from a malformed later member"
        );
    }

    #[tokio::test]
    async fn insert_created_then_same_name_is_typed_name_taken_not_a_500() {
        let (_dir, writer, pool) = open_seeded().await;
        let team = name("Team");
        let created = provision(&writer, &req(&team, user("user-1"), &[], &[]), 10)
            .await
            .unwrap();
        assert!(matches!(created, WorkspaceProvisionOutcome::Provisioned(_)));
        // A second create with the SAME regular name key trips the in-lock name check
        // and is a typed NameTaken — not an Err/500 — and wrote no second workspace.
        let team_lower = name("team");
        let again = provision(&writer, &req(&team_lower, user("user-1"), &[], &[]), 11)
            .await
            .unwrap();
        assert_eq!(again, WorkspaceProvisionOutcome::NameTaken);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM workspaces")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "the loser wrote no workspace row");
    }

    #[tokio::test]
    async fn unknown_member_user_ids_are_silently_filtered() {
        let (_dir, writer, pool) = open_seeded().await;
        let team = name("Team");
        let users = [
            user("user-2"),
            user("ghost-user"),
            user("user-2"),
            user("user-1"),
        ];
        let WorkspaceProvisionOutcome::Provisioned(p) =
            provision(&writer, &req(&team, user("user-1"), &users, &[]), 10)
                .await
                .unwrap()
        else {
            panic!("expected Provisioned");
        };
        let members: Vec<String> = sqlx::query_scalar(
            "SELECT user_id FROM workspace_users WHERE workspace_id=? ORDER BY user_id",
        )
        .bind(p.face.id.as_str())
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(members, vec!["user-1".to_owned(), "user-2".to_owned()]);
        assert_eq!(p.member_users, vec![user("user-1"), user("user-2")]);
    }

    #[tokio::test]
    async fn a_known_but_unowned_agent_refuses_and_writes_nothing() {
        // #28/#41: an agent whose owner != the creator refuses (AgentNotOwned) and the
        // WHOLE boundary rolls back — no workspace/membership/channel row survives.
        let (_dir, writer, pool) = open_seeded().await;
        // Re-own agent-b to a different user so the creator (user-1) does not own it.
        sqlx::query("UPDATE agents SET created_by_user_id='user-2' WHERE id='agent-b'")
            .execute(&pool)
            .await
            .unwrap();
        let team = name("Team");
        let agents = [agent("agent-a"), agent("agent-b")];
        let outcome = provision(&writer, &req(&team, user("user-1"), &[], &agents), 10)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            WorkspaceProvisionOutcome::AgentNotOwned {
                agent_ids: vec![agent("agent-b")]
            }
        );
        // Zero reach: no workspace, no membership, no channel.
        let ws_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM workspaces")
            .fetch_one(&pool)
            .await
            .unwrap();
        let member_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM workspace_members")
            .fetch_one(&pool)
            .await
            .unwrap();
        let chan_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!((ws_rows, member_rows, chan_rows), (0, 0, 0));
    }

    #[tokio::test]
    async fn provision_enrolls_accepted_agents_with_default_capabilities_and_roster() {
        // The consolidated boundary seeds membership + default caps + onboarding roster
        // for each accepted agent, all in the ONE tx.
        let (_dir, writer, pool) = open_seeded().await;
        let team = name("Team");
        let agents = [agent("agent-a"), agent("agent-b")];
        let WorkspaceProvisionOutcome::Provisioned(p) =
            provision(&writer, &req(&team, user("user-1"), &[], &agents), 10)
                .await
                .unwrap()
        else {
            panic!("expected Provisioned");
        };
        let members: Vec<String> = sqlx::query_scalar(
            "SELECT agent_id FROM workspace_members WHERE workspace_id=? ORDER BY agent_id",
        )
        .bind(p.face.id.as_str())
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(members, vec!["agent-a".to_owned(), "agent-b".to_owned()]);
        let caps: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM workspace_agent_capabilities WHERE workspace_id=? AND agent_id='agent-a'",
        )
        .bind(p.face.id.as_str())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            caps, 11,
            "default capability set seeded for the accepted agent"
        );
        let roster: Vec<String> = sqlx::query_scalar(
            "SELECT agent_id FROM conversation_agents WHERE conversation_id=? ORDER BY agent_id",
        )
        .bind(p.onboarding_channel.channel.as_str())
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(roster, vec!["agent-a".to_owned(), "agent-b".to_owned()]);
        // #3: the committed onboarding channel is PRIVATE — both the stored row and the
        // witness the 201 body is built from say so (never a hard-coded "public").
        assert_eq!(p.onboarding_channel.visibility, "private");
        assert_eq!(p.onboarding_channel.title, "Onboarding");
        assert_eq!(p.onboarding_channel.status, "idle");
        let stored_visibility: String =
            sqlx::query_scalar("SELECT visibility FROM conversations WHERE id = ?")
                .bind(p.onboarding_channel.channel.as_str())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored_visibility, "private", "DB row is private");
    }

    #[tokio::test]
    async fn provision_writes_the_first_agents_message_then_subsequent_appends_advance_seq() {
        // `provision_workspace` writes the FIRST accepted agent's
        // onboarding message IN its own tx (seq 1). The route then appends the
        // SUBSEQUENT agents one at a time; the canonical primitive keeps
        // latest_message_seq / head monotone so the next real send takes N+1.
        let (_dir, writer, pool) = open_seeded().await;
        let team = name("Team");
        let agents = [agent("agent-a"), agent("agent-b")];
        let WorkspaceProvisionOutcome::Provisioned(p) =
            provision(&writer, &req(&team, user("user-1"), &[], &agents), 10)
                .await
                .unwrap()
        else {
            panic!("expected Provisioned");
        };
        let channel = p.onboarding_channel.channel.clone();
        // The first accepted agent's message is ALREADY durable (committed in-tx).
        let FirstOnboardingOutcome::Committed(first) = &p.first_onboarding else {
            panic!("a workspace with member agents seeds the first onboarding message in-tx");
        };
        assert_eq!(
            first.agent.as_str(),
            "agent-a",
            "first = request-order head"
        );
        let after_provision: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=?")
                .bind(channel.as_str())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            after_provision, 1,
            "provision committed exactly the first agent's onboarding message"
        );
        // Append the SUBSEQUENT agent (agent-b) as the route does — id-less intent; the
        // writer mints id + samples clock in-lock.
        let intent = AgentOnboardingIntent {
            agent: agent("agent-b"),
            conversation: channel.clone(),
            workspace_name: "Team".to_owned(),
            recipient_handle: ParticipantHandle::parse("bob").unwrap(),
            others: vec![WorkspaceOnboardingPeer {
                handle: ParticipantHandle::parse("alice").unwrap(),
                runtime: "claude".to_owned(),
            }],
        };
        let committed = writer
            .append_agent_onboarding_message(
                &intent,
                &crate::conversation_messaging::messaging::HostEnvironment,
                &TestClock(20),
            )
            .await
            .unwrap();
        // The committed id is the WRITER's mint (the intent carries no id at all, R6 #2).
        assert!(
            !committed.message_id.as_str().is_empty(),
            "the writer minted the id in-lock"
        );
        // N=2 onboarding messages: latest_message_seq == main-line head == 2.
        let latest: i64 =
            sqlx::query_scalar("SELECT latest_message_seq FROM conversations WHERE id=?")
                .bind(channel.as_str())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(latest, 2, "conversations.latest_message_seq == N");
        let head: i64 = sqlx::query_scalar(
            "SELECT latest_seq FROM conversation_stream_heads WHERE conversation_id=? AND thread_root_id IS NULL",
        )
        .bind(channel.as_str())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(head, 2, "main-line stream head == N");
        // The messages actually landed at seq 1,2 (no collision, monotone).
        let seqs: Vec<i64> =
            sqlx::query_scalar("SELECT seq FROM messages WHERE conversation_id=? ORDER BY seq")
                .bind(channel.as_str())
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(seqs, vec![1, 2]);

        // Real-send-N+1: the onboarding writes advanced the SAME per-conversation
        // ordinal a normal send reserves from, so the next send takes N+1.
        let mut conn = pool.acquire().await.unwrap();
        let next =
            crate::conversation_messaging::messaging::reserve_next_seq_on(&mut conn, &channel)
                .await
                .ok()
                .expect("reserve next ordinal");
        assert_eq!(
            next.get(),
            3,
            "next reserved ordinal after N onboarding == N+1"
        );
    }

    #[tokio::test]
    async fn first_onboarding_failure_keeps_setup_residue_with_no_message_half_write() {
        for corrupt_after_insert in [false, true] {
            // The first agent's onboarding message is attempted under a
            // SAVEPOINT inside the provision tx. Cover both failure before insertion
            // (the third entropy mint) and seal failure after a real message insert.
            // Both must roll back only the savepoint: the workspace/members/private-channel
            // SETUP residue still COMMITS, and the messages table + stream head carry NO
            // half-write. The outcome is `Failed` (route → 500, zero deliver/start effect).
            let (_dir, writer, pool) = open_seeded().await;
            if corrupt_after_insert {
                sqlx::query("CREATE TRIGGER corrupt_onboarding AFTER INSERT ON messages BEGIN UPDATE messages SET content = 'corrupted after insert' WHERE id = NEW.id; END")
                .execute(&pool).await.unwrap();
            }
            let team = name("Team");
            let agents = [agent("agent-a"), agent("agent-b")];
            let WorkspaceProvisionOutcome::Provisioned(p) = writer
                .provision_workspace(
                    &req(&team, user("user-1"), &[], &agents),
                    &NthMintFailsEntropy::at(if corrupt_after_insert { usize::MAX } else { 3 }),
                    &TestClock(10),
                )
                .await
                .expect("a first-message fault is NOT a provision Err — setup still commits")
            else {
                panic!("expected Provisioned (setup residue), got a refusal");
            };
            assert_eq!(
                p.first_onboarding,
                FirstOnboardingOutcome::Failed,
                "the first onboarding message failed under the savepoint"
            );
            // Setup residue committed: the workspace, its onboarding channel, membership +
            // caps, and the founding roster are all present.
            let ws_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM workspaces WHERE id=?")
                .bind(p.face.id.as_str())
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(ws_rows, 1, "workspace setup residue is preserved");
            let channel_rows: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM conversations WHERE id=?")
                    .bind(p.onboarding_channel.channel.as_str())
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(channel_rows, 1, "onboarding channel residue is preserved");
            let member_rows: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM conversation_agents WHERE conversation_id=?",
            )
            .bind(p.onboarding_channel.channel.as_str())
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(member_rows, 2, "founding agent roster residue is preserved");
            // NO message half-write: zero message rows, and the stream head/latest_seq is
            // untouched (the savepoint rolled the reserve + insert + head bump ALL back).
            let message_rows: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=?")
                    .bind(p.onboarding_channel.channel.as_str())
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(
                message_rows, 0,
                "no message row survived the savepoint rollback"
            );
            let latest: i64 =
                sqlx::query_scalar("SELECT latest_message_seq FROM conversations WHERE id=?")
                    .bind(p.onboarding_channel.channel.as_str())
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(
                latest, 0,
                "latest_message_seq was not advanced by the rolled-back message"
            );
            // The next reserved ordinal is still 1 — the failed message consumed no seq.
            let mut conn = pool.acquire().await.unwrap();
            let next = crate::conversation_messaging::messaging::reserve_next_seq_on(
                &mut conn,
                &p.onboarding_channel.channel,
            )
            .await
            .ok()
            .expect("reserve");
            assert_eq!(
                next.get(),
                1,
                "the rolled-back first message consumed no ordinal"
            );
        }
    }

    #[tokio::test]
    async fn provision_with_no_member_agents_seeds_no_first_onboarding() {
        // The `NoAgents` arm: a workspace with zero accepted member agents commits the
        // setup and seeds NO onboarding message (no savepoint attempt).
        let (_dir, writer, pool) = open_seeded().await;
        let team = name("Solo");
        let WorkspaceProvisionOutcome::Provisioned(p) =
            provision(&writer, &req(&team, user("user-1"), &[], &[]), 10)
                .await
                .unwrap()
        else {
            panic!("expected Provisioned");
        };
        assert_eq!(p.first_onboarding, FirstOnboardingOutcome::NoAgents);
        let message_rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=?")
                .bind(p.onboarding_channel.channel.as_str())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(message_rows, 0, "no agents ⇒ no onboarding message");
    }

    #[tokio::test]
    async fn onboarding_message_clock_is_sampled_after_the_writer_lock_is_acquired() {
        // The append writer samples its clock AFTER it holds the
        // `BEGIN IMMEDIATE` lock. Prove it with a shared-atomic clock: a competitor
        // holds the write lock, we advance the clock WHILE the append is blocked
        // queueing for the lock, then release; the committed row's created_at must be
        // the POST-release value — never the value at the time the append was issued.
        use std::sync::Arc;
        use std::sync::atomic::{AtomicI64, Ordering};

        #[derive(Clone)]
        struct SharedClock(Arc<AtomicI64>);
        impl silverhand_backend_core::identity_access::workspace_creation::WorkspaceProvisionClock
            for SharedClock
        {
            fn now_ms(&self) -> i64 {
                self.0.load(Ordering::SeqCst)
            }
        }

        let (_dir, writer, pool) = open_seeded().await;
        // Provision the workspace + channel first. It writes agent-a's message (seq 1)
        // in its own tx at the provision clock; that instant is irrelevant here.
        let team = name("Team");
        let agents = [agent("agent-a"), agent("agent-b")];
        let WorkspaceProvisionOutcome::Provisioned(p) = writer
            .provision_workspace(
                &req(&team, user("user-1"), &[], &agents),
                &crate::conversation_messaging::messaging::HostEnvironment,
                &TestClock(10),
            )
            .await
            .unwrap()
        else {
            panic!("expected Provisioned");
        };
        let channel = p.onboarding_channel.channel.clone();

        let clock = SharedClock(Arc::new(AtomicI64::new(100)));
        // A competitor grabs the write lock and holds it.
        let mut blocker = pool.begin_with(ROOT_WRITE_BEGIN).await.unwrap();
        sqlx::query("SELECT 1")
            .fetch_one(&mut *blocker)
            .await
            .unwrap();

        // Spawn the SUBSEQUENT append (agent-b) — it blocks acquiring the writer lock.
        let intent = AgentOnboardingIntent {
            agent: agent("agent-b"),
            conversation: channel.clone(),
            workspace_name: "Team".to_owned(),
            recipient_handle: ParticipantHandle::parse("bob").unwrap(),
            others: vec![],
        };
        let writer2 = writer.clone();
        let clock2 = clock.clone();
        let handle = tokio::spawn(async move {
            writer2
                .append_agent_onboarding_message(
                    &intent,
                    &crate::conversation_messaging::messaging::HostEnvironment,
                    &clock2,
                )
                .await
        });

        // While the append is blocked, advance the clock, THEN release the lock.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        clock.0.store(777, Ordering::SeqCst);
        drop(blocker);

        let committed = handle.await.unwrap().unwrap();
        // Query the created_at of THIS committed message (id minted by the writer).
        let created_at: i64 = sqlx::query_scalar("SELECT created_at FROM messages WHERE id=?")
            .bind(committed.message_id.as_str())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            created_at, 777,
            "the message stamp is the POST-lock clock value, not the 100 at issue time"
        );
    }
}
