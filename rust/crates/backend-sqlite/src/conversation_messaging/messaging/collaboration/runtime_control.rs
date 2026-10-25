//! One-snapshot SQLite admission for conversation runtime control.

use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::{
        RuntimeControlAdmission, RuntimeControlAdmissionScope,
    },
    foundation::identity::{AgentId, ConversationId, UserId},
    identity_access::agents::session_scope::AgentSessionScope,
};
use sqlx::{Row, Sqlite, Transaction};
use thiserror::Error;

use super::super::MessagingStore;

const DEFERRED_READ_BEGIN: &str = "BEGIN DEFERRED";
const SELECT_CONVERSATION_SHAPE: &str =
    "SELECT placement_kind, roster_policy FROM conversations WHERE id = ?";
const SELECT_AGENT_EXISTS: &str = "SELECT 1 FROM agents WHERE id = ?";
// A workspace uses its roster; a DM uses its actual live agent endpoint.
// DM admission additionally requires the immutable agent scope. Human endpoint
// liveness and creator control remain separate questions in the same snapshot.
const SELECT_AGENT_MEMBERSHIP: &str = "SELECT 1 FROM conversations c JOIN agents a ON a.id = ? \
    WHERE c.id = ? AND ((c.placement_kind = 'workspace' AND EXISTS ( \
        SELECT 1 FROM conversation_agents ca WHERE ca.conversation_id=c.id AND ca.agent_id=a.id)) \
    OR (c.placement_kind='direct_pair' AND a.session_scope='agent' AND ( \
        (c.endpoint_a_kind='agent' AND c.endpoint_a_id=a.id AND c.endpoint_a_deleted_at IS NULL) \
        OR (c.endpoint_b_kind='agent' AND c.endpoint_b_id=a.id AND c.endpoint_b_deleted_at IS NULL))))";
const SELECT_CONVERSATION_POST_FACTS: &str = "SELECT placement_kind, workspace_id, \
    endpoint_a_kind, endpoint_a_id, endpoint_a_deleted_at, endpoint_b_kind, endpoint_b_id, \
    endpoint_b_deleted_at FROM conversations WHERE id = ?";
const SELECT_WORKSPACE_HUMAN_PARTICIPATION: &str = "SELECT \
    EXISTS(SELECT 1 FROM workspace_users WHERE workspace_id = ? AND user_id = ?) \
    AND EXISTS(SELECT 1 FROM conversation_users WHERE conversation_id = ? AND user_id = ?) \
    AS participates";
const SELECT_AGENT_CREATOR: &str = "SELECT created_by_user_id FROM agents WHERE id = ?";
const SELECT_AGENT_SESSION_SCOPE: &str = "SELECT session_scope FROM agents WHERE id = ?";
const SELECT_USER_EXISTS: &str = "SELECT 1 FROM users WHERE id = ?";

#[derive(Clone)]
pub struct SqliteRuntimeControlAdmission {
    pool: crate::foundation::database::SqliteDatabase,
}

#[derive(Debug, Error)]
pub enum RuntimeControlAdmissionStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored conversation placement is outside the closed runtime-control set")]
    InvalidConversationPlacement,
    #[error("stored direct-pair endpoint is malformed")]
    MalformedDirectPairEndpoint,
    #[error("stored agent session scope is outside the declared vocabulary")]
    UnrecognizedSessionScope,
}

impl<E> MessagingStore<E> {
    pub fn runtime_control_admission(&self) -> SqliteRuntimeControlAdmission {
        SqliteRuntimeControlAdmission {
            pool: self.pool.clone(),
        }
    }
}

impl RuntimeControlAdmission for SqliteRuntimeControlAdmission {
    type Error = RuntimeControlAdmissionStoreError;
    type Scope = SqliteRuntimeControlSnapshot;

    async fn begin_snapshot(&self) -> Result<Self::Scope, Self::Error> {
        Ok(SqliteRuntimeControlSnapshot {
            tx: self.pool.begin_with(DEFERRED_READ_BEGIN).await?,
        })
    }
}

pub struct SqliteRuntimeControlSnapshot {
    tx: Transaction<'static, Sqlite>,
}

impl RuntimeControlAdmissionScope for SqliteRuntimeControlSnapshot {
    type Error = RuntimeControlAdmissionStoreError;

    async fn conversation_exists(
        &mut self,
        conversation: &ConversationId,
    ) -> Result<bool, Self::Error> {
        let Some(row) = sqlx::query(SELECT_CONVERSATION_SHAPE)
            .bind(conversation.as_str())
            .fetch_optional(&mut *self.tx)
            .await?
        else {
            return Ok(false);
        };
        let placement: String = row.try_get("placement_kind")?;
        let roster: String = row.try_get("roster_policy")?;
        match (placement.as_str(), roster.as_str()) {
            ("workspace", "open" | "fixed") | ("direct_pair", "fixed") => Ok(true),
            _ => Err(RuntimeControlAdmissionStoreError::InvalidConversationPlacement),
        }
    }

    async fn agent_exists(&mut self, agent: &AgentId) -> Result<bool, Self::Error> {
        Ok(sqlx::query(SELECT_AGENT_EXISTS)
            .bind(agent.as_str())
            .fetch_optional(&mut *self.tx)
            .await?
            .is_some())
    }

    async fn agent_is_member(
        &mut self,
        conversation: &ConversationId,
        agent: &AgentId,
    ) -> Result<bool, Self::Error> {
        Ok(sqlx::query(SELECT_AGENT_MEMBERSHIP)
            .bind(agent.as_str())
            .bind(conversation.as_str())
            .fetch_optional(&mut *self.tx)
            .await?
            .is_some())
    }

    async fn actor_may_post(
        &mut self,
        conversation: &ConversationId,
        actor: &UserId,
    ) -> Result<bool, Self::Error> {
        let Some(row) = sqlx::query(SELECT_CONVERSATION_POST_FACTS)
            .bind(conversation.as_str())
            .fetch_optional(&mut *self.tx)
            .await?
        else {
            return Ok(false);
        };
        let placement: String = row.try_get("placement_kind")?;
        match placement.as_str() {
            "workspace" => {
                let workspace: Option<String> = row.try_get("workspace_id")?;
                let Some(workspace) = workspace else {
                    return Err(RuntimeControlAdmissionStoreError::InvalidConversationPlacement);
                };
                let participation: i64 = sqlx::query_scalar(SELECT_WORKSPACE_HUMAN_PARTICIPATION)
                    .bind(workspace)
                    .bind(actor.as_str())
                    .bind(conversation.as_str())
                    .bind(actor.as_str())
                    .fetch_one(&mut *self.tx)
                    .await?;
                Ok(participation != 0)
            }
            "direct_pair" => {
                let endpoint_a = decode_endpoint(&row, "endpoint_a")?;
                let endpoint_b = decode_endpoint(&row, "endpoint_b")?;
                let actor_participates = endpoint_a.is_human(actor) || endpoint_b.is_human(actor);
                if !actor_participates {
                    return Ok(false);
                }
                Ok(endpoint_a.is_live(&mut self.tx).await?
                    && endpoint_b.is_live(&mut self.tx).await?)
            }
            _ => Err(RuntimeControlAdmissionStoreError::InvalidConversationPlacement),
        }
    }

    async fn actor_may_edit_agent(
        &mut self,
        actor: &UserId,
        agent: &AgentId,
    ) -> Result<bool, Self::Error> {
        let creator: Option<String> = sqlx::query_scalar(SELECT_AGENT_CREATOR)
            .bind(agent.as_str())
            .fetch_optional(&mut *self.tx)
            .await?;
        Ok(creator.as_deref() == Some(actor.as_str()))
    }

    async fn agent_session_scope(
        &mut self,
        agent: &AgentId,
    ) -> Result<Option<AgentSessionScope>, Self::Error> {
        let Some(raw): Option<String> = sqlx::query_scalar(SELECT_AGENT_SESSION_SCOPE)
            .bind(agent.as_str())
            .fetch_optional(&mut *self.tx)
            .await?
        else {
            // No agent row: the seat operation refuses as AgentNotFound. An
            // unrecognized token, by contrast, is a stored fault (below), never
            // silently collapsed to this absent case.
            return Ok(None);
        };
        AgentSessionScope::parse(&raw)
            .map(Some)
            .ok_or(RuntimeControlAdmissionStoreError::UnrecognizedSessionScope)
    }

    async fn finish(self) -> Result<(), Self::Error> {
        self.tx.commit().await?;
        Ok(())
    }
}

enum Endpoint {
    Human { id: String, deleted_at: Option<i64> },
    Agent { id: String, deleted_at: Option<i64> },
}

impl Endpoint {
    fn is_human(&self, actor: &UserId) -> bool {
        matches!(self, Self::Human { id, .. } if id == actor.as_str())
    }

    async fn is_live(
        &self,
        tx: &mut Transaction<'static, Sqlite>,
    ) -> Result<bool, RuntimeControlAdmissionStoreError> {
        let (query, id, deleted_at) = match self {
            Self::Human { id, deleted_at } => (SELECT_USER_EXISTS, id, deleted_at),
            Self::Agent { id, deleted_at } => (SELECT_AGENT_EXISTS, id, deleted_at),
        };
        if deleted_at.is_some() {
            return Ok(false);
        }
        Ok(sqlx::query(query)
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?
            .is_some())
    }
}

fn decode_endpoint(
    row: &sqlx::sqlite::SqliteRow,
    prefix: &'static str,
) -> Result<Endpoint, RuntimeControlAdmissionStoreError> {
    let (kind_field, id_field, deleted_field) = match prefix {
        "endpoint_a" => ("endpoint_a_kind", "endpoint_a_id", "endpoint_a_deleted_at"),
        "endpoint_b" => ("endpoint_b_kind", "endpoint_b_id", "endpoint_b_deleted_at"),
        _ => unreachable!("endpoint prefix is module-owned"),
    };
    let kind: Option<String> = row.try_get(kind_field)?;
    let id: Option<String> = row.try_get(id_field)?;
    let deleted_at: Option<i64> = row.try_get(deleted_field)?;
    let id = id
        .filter(|value| !value.is_empty())
        .ok_or(RuntimeControlAdmissionStoreError::MalformedDirectPairEndpoint)?;
    match kind.as_deref() {
        Some("human") => Ok(Endpoint::Human { id, deleted_at }),
        Some("agent") => Ok(Endpoint::Agent { id, deleted_at }),
        _ => Err(RuntimeControlAdmissionStoreError::MalformedDirectPairEndpoint),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use silverhand_backend_core::{
        conversation_messaging::messaging::collaboration::{
            ClearSessionOutcome, ClearSessionProviderRefusal, HardStopRuntimeOutcome,
            HardStopRuntimeProviderRefusal, RuntimeControlAdmissionRefusal,
            RuntimeControlAdmissionScope, RuntimeControlPort, RuntimeSeatTarget,
            RuntimeStartTarget, StartRuntimeOutcome, StartRuntimeProviderRefusal, start_runtime,
        },
        foundation::identity::{AgentId, ConversationId, UserId},
    };
    use sqlx::{
        Connection, Executor, SqliteConnection,
        sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
    };

    use super::*;

    async fn seeded() -> (tempfile::TempDir, MessagingStore) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runtime-control.sqlite");
        let mut db = crate::current_schema::create_current_schema(&path).await;
        db.execute(sqlx::raw_sql(
            "INSERT INTO users (id, name, display_name, created_at, updated_at) VALUES \
                 ('user-1', 'owner', 'Owner', 0, 0), ('user-2', 'other', 'Other', 0, 0); \
             INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1', 'Host', 'local', 0); \
             INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, \
                 display_name, created_by_user_id, materialization_status, session_scope) VALUES \
                 ('agent-1', 'agent', 'codex', 'host-1', 0, 0, 'Agent', 'user-1', 'active', 'conversation'), \
                 ('agent-2', 'agent-two', 'codex', 'host-1', 0, 0, 'Agent Two', 'user-1', 'active', 'conversation'); \
             INSERT INTO workspaces (id, name, name_key, owner_user_id, created_at, updated_at) \
                 VALUES ('workspace-1', 'Space', 'space', 'user-1', 0, 0); \
             INSERT INTO workspace_users (workspace_id, user_id, joined_at, role) VALUES \
                 ('workspace-1', 'user-1', 0, 'owner'), ('workspace-1', 'user-2', 0, 'member'); \
             INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, \
                 title_key, status, visibility, created_at, updated_at) VALUES \
                 ('conversation-1', 'workspace-1', 'workspace', 'open', 'General', 'general', \
                  'open', 'private', 0, 0), \
                 ('conversation-fixed', 'workspace-1', 'workspace', 'fixed', 'Fixed', 'fixed', \
                  'open', 'private', 0, 0); \
             INSERT INTO conversations (id, workspace_id, endpoint_a_kind, endpoint_a_id, \
                 endpoint_b_kind, endpoint_b_id, placement_kind, roster_policy, title, title_key, \
                 status, visibility, created_at, updated_at) VALUES \
                 ('pair-1', NULL, 'agent', 'agent-2', 'human', 'user-1', 'direct_pair', 'fixed', \
                  'Direct Pair', NULL, 'idle', 'private', 0, 0); \
             INSERT INTO conversation_users (conversation_id, user_id, joined_at) VALUES \
                 ('conversation-1', 'user-1', 0), ('conversation-fixed', 'user-1', 0); \
             INSERT INTO conversation_agents (conversation_id, agent_id, joined_at) VALUES \
                 ('conversation-1', 'agent-1', 0), ('conversation-fixed', 'agent-1', 0);",
        ))
        .await
        .unwrap();
        db.close().await.unwrap();
        let store = MessagingStore::open(&path).await.unwrap();
        (directory, store)
    }

    fn target() -> RuntimeStartTarget {
        RuntimeStartTarget::new(
            UserId::parse("user-1").unwrap(),
            AgentId::parse("agent-1").unwrap(),
            ConversationId::parse("conversation-1").unwrap(),
        )
    }

    #[tokio::test]
    async fn concurrent_revocation_cannot_tear_the_five_question_snapshot() {
        let (directory, store) = seeded().await;
        let admission = store.runtime_control_admission();
        let mut snapshot = admission.begin_snapshot().await.unwrap();
        let target = target();

        assert!(
            snapshot
                .conversation_exists(target.presentation_origin())
                .await
                .unwrap()
        );

        let path = directory.path().join("runtime-control.sqlite");
        let mut writer = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
            .await
            .unwrap();
        writer
            .execute(sqlx::raw_sql(
                "DELETE FROM conversation_agents WHERE conversation_id='conversation-1' AND agent_id='agent-1'; \
                 DELETE FROM conversation_users WHERE conversation_id='conversation-1' AND user_id='user-1';",
            ))
            .await
            .unwrap();
        writer.close().await.unwrap();

        assert!(snapshot.agent_exists(target.agent()).await.unwrap());
        assert!(
            snapshot
                .agent_is_member(target.presentation_origin(), target.agent())
                .await
                .unwrap()
        );
        assert!(
            snapshot
                .actor_may_post(target.presentation_origin(), target.actor())
                .await
                .unwrap()
        );
        assert!(
            snapshot
                .actor_may_edit_agent(target.actor(), target.agent())
                .await
                .unwrap()
        );
        snapshot.finish().await.unwrap();

        let mut fresh = admission.begin_snapshot().await.unwrap();
        assert!(
            !fresh
                .agent_is_member(target.presentation_origin(), target.agent())
                .await
                .unwrap()
        );
        assert!(
            !fresh
                .actor_may_post(target.presentation_origin(), target.actor())
                .await
                .unwrap()
        );
        fresh.finish().await.unwrap();
    }

    struct AcquiringProvider {
        pool: sqlx::SqlitePool,
        calls: Arc<Mutex<usize>>,
    }

    #[derive(Debug, Error)]
    enum AcquireProviderFailure {
        #[error("timed out acquiring the admission pool after snapshot finish")]
        Timeout,
        #[error("database operation failed")]
        Sql(#[from] sqlx::Error),
    }

    impl RuntimeControlPort for AcquiringProvider {
        type Error = AcquireProviderFailure;

        async fn start(
            &self,
            _target: &RuntimeStartTarget,
        ) -> Result<Result<StartRuntimeOutcome, StartRuntimeProviderRefusal>, Self::Error> {
            let connection = tokio::time::timeout(Duration::from_secs(1), self.pool.acquire())
                .await
                .map_err(|_| AcquireProviderFailure::Timeout)??;
            drop(connection);
            *self.calls.lock().unwrap() += 1;
            Ok(Ok(StartRuntimeOutcome))
        }

        async fn hard_stop(
            &self,
            _target: &RuntimeSeatTarget,
        ) -> Result<
            Result<
                silverhand_backend_core::conversation_messaging::messaging::collaboration::HardStopRuntimeOutcome,
                silverhand_backend_core::conversation_messaging::messaging::collaboration::HardStopRuntimeProviderRefusal,
            >,
            Self::Error,
        >{
            unreachable!()
        }

        async fn clear_session(
            &self,
            _target: &RuntimeSeatTarget,
        ) -> Result<
            Result<
                silverhand_backend_core::conversation_messaging::messaging::collaboration::ClearSessionOutcome,
                silverhand_backend_core::conversation_messaging::messaging::collaboration::ClearSessionProviderRefusal,
            >,
            Self::Error,
        >{
            unreachable!()
        }
    }

    #[tokio::test]
    async fn finish_returns_the_only_pool_connection_before_the_provider_runs() {
        let (directory, store) = seeded().await;
        drop(store);
        let options = SqliteConnectOptions::new()
            .filename(directory.path().join("runtime-control.sqlite"))
            .create_if_missing(false)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .busy_timeout(super::super::super::BUSY_TIMEOUT);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        let admission = SqliteRuntimeControlAdmission {
            pool: pool.clone().into(),
        };
        let calls = Arc::new(Mutex::new(0));
        let provider = AcquiringProvider {
            pool,
            calls: Arc::clone(&calls),
        };
        assert_eq!(
            start_runtime(&admission, &provider, target())
                .await
                .unwrap(),
            Ok(StartRuntimeOutcome)
        );
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[derive(Default)]
    struct CountingProvider(Arc<Mutex<usize>>);

    impl RuntimeControlPort for CountingProvider {
        type Error = std::io::Error;

        async fn start(
            &self,
            _target: &RuntimeStartTarget,
        ) -> Result<Result<StartRuntimeOutcome, StartRuntimeProviderRefusal>, Self::Error> {
            *self.0.lock().unwrap() += 1;
            Ok(Ok(StartRuntimeOutcome))
        }

        async fn hard_stop(
            &self,
            _target: &RuntimeSeatTarget,
        ) -> Result<Result<HardStopRuntimeOutcome, HardStopRuntimeProviderRefusal>, Self::Error>
        {
            unreachable!()
        }

        async fn clear_session(
            &self,
            _target: &RuntimeSeatTarget,
        ) -> Result<Result<ClearSessionOutcome, ClearSessionProviderRefusal>, Self::Error> {
            unreachable!()
        }
    }

    fn target_for(actor: &str, conversation: &str, agent: &str) -> RuntimeStartTarget {
        RuntimeStartTarget::new(
            UserId::parse(actor).unwrap(),
            AgentId::parse(agent).unwrap(),
            ConversationId::parse(conversation).unwrap(),
        )
    }

    #[tokio::test]
    async fn fixed_workspace_seats_admit_while_a_conversation_scoped_dm_refuses() {
        let (_directory, store) = seeded().await;
        let provider = CountingProvider::default();
        assert_eq!(
            start_runtime(
                &store.runtime_control_admission(),
                &provider,
                target_for("user-1", "conversation-fixed", "agent-1"),
            )
            .await
            .unwrap(),
            Ok(StartRuntimeOutcome)
        );
        assert_eq!(*provider.0.lock().unwrap(), 1);

        assert_eq!(
            start_runtime(
                &store.runtime_control_admission(),
                &provider,
                target_for("user-1", "pair-1", "agent-2"),
            )
            .await
            .unwrap(),
            Err(
                silverhand_backend_core::conversation_messaging::messaging::collaboration::StartRuntimeRefusal::Admission(
                    RuntimeControlAdmissionRefusal::AgentNotMember,
                )
            )
        );
        assert_eq!(*provider.0.lock().unwrap(), 1);
    }

    async fn direct_pair_post(
        admission: &SqliteRuntimeControlAdmission,
        actor: &str,
    ) -> Result<bool, RuntimeControlAdmissionStoreError> {
        let mut snapshot = admission.begin_snapshot().await?;
        let result = snapshot
            .actor_may_post(
                &ConversationId::parse("pair-1").unwrap(),
                &UserId::parse(actor).unwrap(),
            )
            .await;
        let finish = snapshot.finish().await;
        match result {
            Ok(value) => {
                finish?;
                Ok(value)
            }
            Err(cause) => Err(cause),
        }
    }

    #[tokio::test]
    async fn direct_pair_post_authority_is_endpoint_scoped_and_fails_closed_on_liveness() {
        let (directory, store) = seeded().await;
        let admission = store.runtime_control_admission();
        assert!(direct_pair_post(&admission, "user-1").await.unwrap());
        assert!(!direct_pair_post(&admission, "user-2").await.unwrap());

        let path = directory.path().join("runtime-control.sqlite");
        let mut writer = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
            .await
            .unwrap();
        sqlx::query("UPDATE conversations SET endpoint_a_deleted_at=1 WHERE id='pair-1'")
            .execute(&mut writer)
            .await
            .unwrap();
        writer.close().await.unwrap();
        assert!(!direct_pair_post(&admission, "user-1").await.unwrap());

        let (missing_directory, missing_store) = seeded().await;
        let missing_admission = missing_store.runtime_control_admission();
        let missing_path = missing_directory.path().join("runtime-control.sqlite");
        let mut writer = SqliteConnection::connect(&format!("sqlite:{}", missing_path.display()))
            .await
            .unwrap();
        writer
            .execute(sqlx::raw_sql(
                "DROP TRIGGER trg_agents_stamp_endpoint_deleted; DELETE FROM agents WHERE id='agent-2';",
            ))
            .await
            .unwrap();
        writer.close().await.unwrap();
        assert!(
            !direct_pair_post(&missing_admission, "user-1")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn malformed_direct_pair_endpoint_is_a_typed_store_failure() {
        let (directory, store) = seeded().await;
        let admission = store.runtime_control_admission();
        let path = directory.path().join("runtime-control.sqlite");
        let mut writer = SqliteConnection::connect(&format!("sqlite:{}", path.display()))
            .await
            .unwrap();
        writer
            .execute(sqlx::raw_sql(
                "DROP TRIGGER trg_conversations_endpoints_immutable; \
                 PRAGMA ignore_check_constraints = ON; \
                 UPDATE conversations SET endpoint_a_kind='bogus' WHERE id='pair-1';",
            ))
            .await
            .unwrap();
        writer.close().await.unwrap();
        assert!(matches!(
            direct_pair_post(&admission, "user-1").await,
            Err(RuntimeControlAdmissionStoreError::MalformedDirectPairEndpoint)
        ));
    }
}
