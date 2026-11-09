//! SQLite authority and get-or-create adapter for human direct-pair initiation.
//!
//! Visibility and initiation consume the same candidate authority as mention-CC.
//! Pair allocation uses the canonical shared mint, whose autocommit path reserves
//! the writer and reads the winning durable row before it commits.

use std::{error::Error, future::Future};

use silverhand_backend_core::{
    conversation_messaging::messaging::{
        DirectPairOpenedPort, PrincipalIdentity, SendClock, SendEntropy,
        collaboration::direct_pair_initiation::{
            AuthorizedDirectPairInitiation, DirectPairFirstSendError,
            DirectPairInitiationAuthority, DirectPairInitiationDecision,
            DirectPairInitiationGrantFact, DirectPairInitiationPort, DirectPairInitiationSource,
            OpenedDirectPairFirstMessage, open_and_send_direct_pair,
        },
    },
    foundation::identity::UserId,
    foundation::persistence::PersistenceFailure,
    identity_access::agents::session_scope::AgentSessionScope,
};

use super::super::authorities::{DirectPairInitiationTarget, direct_pair_initiation_candidate};
use super::super::{AutocommitDirectPairMint, MessagingStore};

#[derive(Clone)]
pub struct SqliteDirectPairInitiation<E, O> {
    store: MessagingStore<E>,
    opened: O,
}

impl<E, O> DirectPairInitiationSource<O> for MessagingStore<E>
where
    E: SendClock + SendEntropy + Send + Sync + 'static,
    O: DirectPairOpenedPort + 'static,
{
    type Port = SqliteDirectPairInitiation<E, O>;

    fn bind_initiation(self, opened: O) -> Self::Port {
        SqliteDirectPairInitiation {
            store: self,
            opened,
        }
    }
}

impl<E, O> DirectPairInitiationAuthority for SqliteDirectPairInitiation<E, O>
where
    E: Send + Sync,
    O: Send + Sync,
{
    type Error = PersistenceFailure;

    async fn decide(
        &self,
        viewer: &UserId,
        target: &PrincipalIdentity,
    ) -> Result<DirectPairInitiationDecision, Self::Error> {
        if let PrincipalIdentity::Human(target_user) = target {
            if target_user == viewer {
                return Ok(DirectPairInitiationDecision::Forbidden);
            }
            let both_exist: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM users WHERE id = ?) \
                 AND EXISTS(SELECT 1 FROM users WHERE id = ?)",
            )
            .bind(viewer.as_str())
            .bind(target_user.as_str())
            .fetch_one(&self.store.pool)
            .await
            .map_err(PersistenceFailure::from_source)?;
            return Ok(if both_exist {
                DirectPairInitiationDecision::Granted(DirectPairInitiationGrantFact::granted(
                    viewer.clone(),
                    target.clone(),
                ))
            } else {
                DirectPairInitiationDecision::NotFound
            });
        }
        let Some(candidate) = direct_pair_initiation_candidate(
            &self.store.pool,
            viewer,
            DirectPairInitiationTarget::Id(target.as_str()),
        )
        .await?
        else {
            return Ok(DirectPairInitiationDecision::NotFound);
        };
        if candidate.scope != AgentSessionScope::Agent {
            return Ok(DirectPairInitiationDecision::Forbidden);
        }
        Ok(DirectPairInitiationDecision::Granted(
            DirectPairInitiationGrantFact::granted(
                candidate.viewer,
                PrincipalIdentity::Agent(candidate.target),
            ),
        ))
    }
}

impl<E, O> SqliteDirectPairInitiation<E, O>
where
    E: SendClock + SendEntropy + Send + Sync,
    O: DirectPairOpenedPort,
{
    /// Enter the canonical mint and then let the server compose the already
    /// existing send UoW through `send`; this adapter owns no second message
    /// persistence path.
    pub async fn open_and_send<F, Fut, T, S>(
        &self,
        authorized: AuthorizedDirectPairInitiation,
        send: F,
    ) -> Result<T, DirectPairFirstSendError<PersistenceFailure, O::Error, S>>
    where
        F: FnOnce(OpenedDirectPairFirstMessage) -> Fut,
        Fut: Future<Output = Result<T, S>>,
        S: Error + Send + Sync + 'static,
    {
        let mut mint = AutocommitDirectPairMint { store: &self.store };
        open_and_send_direct_pair(
            authorized,
            &mut mint,
            &self.opened,
            &self.store.environment,
            send,
        )
        .await
    }
}

impl<E, O> DirectPairInitiationPort for SqliteDirectPairInitiation<E, O>
where
    E: SendClock + SendEntropy + Send + Sync,
    O: DirectPairOpenedPort,
{
    type MintError = PersistenceFailure;
    type OpenedError = O::Error;

    async fn open_and_send<F, Fut, T, S>(
        &self,
        authorized: AuthorizedDirectPairInitiation,
        send: F,
    ) -> Result<T, DirectPairFirstSendError<Self::MintError, Self::OpenedError, S>>
    where
        F: FnOnce(OpenedDirectPairFirstMessage) -> Fut + Send,
        Fut: Future<Output = Result<T, S>> + Send,
        T: Send,
        S: Error + Send + Sync + 'static,
    {
        SqliteDirectPairInitiation::open_and_send(self, authorized, send).await
    }
}

#[cfg(test)]
mod tests {
    use silverhand_backend_core::foundation::identity::AgentId;
    use std::{
        convert::Infallible,
        error::Error,
        sync::{Arc, Mutex},
    };

    use silverhand_backend_core::conversation_messaging::messaging::{
        DirectPairOpenedFact,
        collaboration::direct_pair_initiation::{
            DirectPairInitiationRefusal, authorize_direct_pair_initiation,
        },
    };
    use tempfile::TempDir;

    use super::*;
    use crate::conversation_messaging::messaging::{
        HostEnvironment, MessagingStoreError, POOL_CONNECTIONS,
    };

    #[derive(Clone, Default)]
    struct Opened(Arc<Mutex<Vec<DirectPairOpenedFact>>>);

    impl DirectPairOpenedPort for Opened {
        type Error = Infallible;

        async fn direct_pair_opened(&self, fact: &DirectPairOpenedFact) -> Result<(), Self::Error> {
            self.0.lock().unwrap().push(fact.clone());
            Ok(())
        }
    }

    struct Fixture {
        _directory: TempDir,
        store: MessagingStore,
        opened: Opened,
    }

    impl Fixture {
        async fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("direct-pair.sqlite");
            let mut db = crate::current_schema::create_current_schema(&path).await;
            for statement in [
                "INSERT INTO hosts(id,name,kind,created_at) VALUES('host-1','host','local',0)",
                "INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES\
                 ('user-1','one','One',0,0),('user-2','two','Two',0,0)",
                concat!(
                    seed_agent!(
                        id = "agent-owner",
                        name = "owner",
                        host_id = "host-1",
                        display_name = "Owner",
                        created_by_user_id = "user-1",
                        session_scope = "agent",
                        created_at = 0,
                        updated_at = 0,
                    ),
                    "; ",
                    seed_agent!(
                        id = "agent-frame",
                        name = "frame",
                        host_id = "host-1",
                        display_name = "Frame",
                        created_by_user_id = "user-1",
                        created_at = 0,
                        updated_at = 0,
                    ),
                    "; ",
                    seed_agent!(
                        id = "agent-hidden",
                        name = "hidden",
                        host_id = "host-1",
                        display_name = "Hidden",
                        created_by_user_id = "user-2",
                        session_scope = "agent",
                        created_at = 0,
                        updated_at = 0,
                    ),
                    "; ",
                    seed_agent!(
                        id = "agent-workspace",
                        name = "workspace",
                        host_id = "host-1",
                        display_name = "Workspace",
                        created_by_user_id = "user-2",
                        session_scope = "agent",
                        created_at = 0,
                        updated_at = 0,
                    ),
                    "; ",
                    seed_agent!(
                        id = "agent-pair",
                        name = "pair",
                        host_id = "host-1",
                        display_name = "Pair",
                        created_by_user_id = "user-2",
                        session_scope = "agent",
                        created_at = 0,
                        updated_at = 0,
                    ),
                ),
                "INSERT INTO workspaces(id,name,name_key,created_at,updated_at) \
                 VALUES('workspace-1','W','w',0,0)",
                "INSERT INTO workspace_members(workspace_id,agent_id,joined_at) \
                 VALUES('workspace-1','agent-workspace',0)",
                "INSERT INTO workspace_users(workspace_id,user_id,role,joined_at) \
                 VALUES('workspace-1','user-1','member',0)",
                seed_direct_pair!(
                    id = "existing-pair",
                    endpoint_a_kind = "agent",
                    endpoint_a_id = "agent-pair",
                    endpoint_b_kind = "human",
                    endpoint_b_id = "user-1",
                    created_at = 0,
                    updated_at = 0,
                ),
            ] {
                sqlx::query(statement).execute(&mut db).await.unwrap();
            }
            drop(db);
            let store = MessagingStore::open(&path).await.unwrap();
            Self {
                _directory: directory,
                store,
                opened: Opened::default(),
            }
        }

        fn adapter(&self) -> SqliteDirectPairInitiation<HostEnvironment, Opened> {
            self.store.clone().bind_initiation(self.opened.clone())
        }
    }

    fn user(raw: &str) -> UserId {
        UserId::parse(raw).unwrap()
    }

    fn agent(raw: &str) -> PrincipalIdentity {
        PrincipalIdentity::Agent(AgentId::parse(raw).unwrap())
    }

    async fn authorize(
        adapter: &SqliteDirectPairInitiation<HostEnvironment, Opened>,
        target: &str,
    ) -> AuthorizedDirectPairInitiation {
        authorize_direct_pair_initiation(adapter, user("user-1"), agent(target))
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn creator_workspace_and_existing_pair_arms_share_one_initiation_authority() {
        let fixture = Fixture::new().await;
        let adapter = fixture.adapter();
        for target in ["agent-owner", "agent-workspace", "agent-pair"] {
            let granted = authorize(&adapter, target).await;
            assert_eq!(granted.viewer().as_str(), "user-1");
            assert_eq!(granted.target().as_str(), target);
        }
        assert_eq!(
            authorize_direct_pair_initiation(&adapter, user("user-1"), agent("agent-frame"))
                .await
                .unwrap(),
            Err(DirectPairInitiationRefusal::Forbidden)
        );
        for target in ["agent-hidden", "missing"] {
            assert_eq!(
                authorize_direct_pair_initiation(&adapter, user("user-1"), agent(target))
                    .await
                    .unwrap(),
                Err(DirectPairInitiationRefusal::NotFound)
            );
        }
    }

    #[tokio::test]
    async fn winner_and_conflict_loser_read_the_same_durable_pair_and_only_winner_publishes() {
        let fixture = Fixture::new().await;
        let adapter = fixture.adapter();
        let first = adapter
            .open_and_send(
                authorize(&adapter, "agent-owner").await,
                |fact| async move { Ok::<_, Infallible>(fact.conversation().clone()) },
            )
            .await
            .unwrap();
        let second = adapter
            .open_and_send(
                authorize(&adapter, "agent-owner").await,
                |fact| async move { Ok::<_, Infallible>(fact.conversation().clone()) },
            )
            .await
            .unwrap();
        assert_eq!(first, second);
        let facts = fixture.opened.0.lock().unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].conversation(), &first);
        assert_eq!(facts[0].participants().first().as_str(), "agent-owner");
        assert_eq!(facts[0].participants().second().as_str(), "user-1");
    }

    #[tokio::test]
    async fn concurrent_humans_open_one_pair_and_publish_both_human_endpoints() {
        let fixture = Fixture::new().await;
        let adapter = fixture.adapter();
        let forward = authorize_direct_pair_initiation(
            &adapter,
            user("user-1"),
            PrincipalIdentity::Human(user("user-2")),
        )
        .await
        .unwrap()
        .unwrap();
        let reverse = authorize_direct_pair_initiation(
            &adapter,
            user("user-2"),
            PrincipalIdentity::Human(user("user-1")),
        )
        .await
        .unwrap()
        .unwrap();

        let (first, second) = tokio::join!(
            adapter.open_and_send(forward, |fact| async move {
                Ok::<_, Infallible>(fact.conversation().clone())
            }),
            adapter.open_and_send(reverse, |fact| async move {
                Ok::<_, Infallible>(fact.conversation().clone())
            }),
        );
        assert_eq!(first.unwrap(), second.unwrap());
        let events = fixture.opened.0.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].participants().first(),
            &PrincipalIdentity::Human(user("user-1"))
        );
        assert_eq!(
            events[0].participants().second(),
            &PrincipalIdentity::Human(user("user-2"))
        );
    }

    async fn corrupt_winner_then_repair(trigger_sql: &'static str, expected_field: &'static str) {
        let fixture = Fixture::new().await;
        let adapter = fixture.adapter();
        sqlx::query("DROP TRIGGER trg_conversations_endpoints_immutable")
            .execute(&fixture.store.pool)
            .await
            .unwrap();
        let mut connections = Vec::new();
        for _ in 0..POOL_CONNECTIONS {
            let mut connection = fixture.store.pool.acquire().await.unwrap();
            sqlx::query("PRAGMA ignore_check_constraints = ON")
                .execute(&mut *connection)
                .await
                .unwrap();
            connections.push(connection);
        }
        drop(connections);
        sqlx::query(trigger_sql)
            .execute(&fixture.store.pool)
            .await
            .unwrap();

        let error = adapter
            .open_and_send(authorize(&adapter, "agent-owner").await, |_| async {
                Ok::<(), Infallible>(())
            })
            .await
            .unwrap_err();
        let mut source: &(dyn Error + 'static) = &error;
        let mut field = None;
        let mut chain = Vec::new();
        loop {
            chain.push(source.to_string());
            if let Some(MessagingStoreError::MalformedIdentity { field: found }) =
                source.downcast_ref::<MessagingStoreError>()
            {
                field = Some(*found);
            }
            match source.source() {
                Some(next) => source = next,
                None => break,
            }
        }
        assert_eq!(field, Some(expected_field), "source chain: {chain:?}");
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM conversations WHERE endpoint_a_id='agent-owner' \
             OR endpoint_b_id='agent-owner'",
        )
        .fetch_one(&fixture.store.pool)
        .await
        .unwrap();
        assert_eq!(count, 0, "corrupt winner must roll back as a whole");

        sqlx::query("DROP TRIGGER corrupt_pair")
            .execute(&fixture.store.pool)
            .await
            .unwrap();
        adapter
            .open_and_send(authorize(&adapter, "agent-owner").await, |_| async {
                Ok::<(), Infallible>(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn half_null_selected_endpoint_is_named_corruption_and_rolls_back() {
        corrupt_winner_then_repair(
            "CREATE TRIGGER corrupt_pair AFTER INSERT ON conversations \
             WHEN NEW.placement_kind='direct_pair' BEGIN UPDATE conversations SET \
             endpoint_b_id=NULL WHERE id=NEW.id; END",
            "endpoint_b_kind",
        )
        .await;
    }

    #[tokio::test]
    async fn unknown_selected_endpoint_kind_is_named_corruption_and_rolls_back() {
        corrupt_winner_then_repair(
            "CREATE TRIGGER corrupt_pair AFTER INSERT ON conversations \
             WHEN NEW.placement_kind='direct_pair' BEGIN UPDATE conversations SET \
             endpoint_b_kind='intruder' WHERE id=NEW.id; END",
            "endpoint_b_kind",
        )
        .await;
    }

    #[tokio::test]
    async fn empty_selected_endpoint_id_is_named_corruption_and_rolls_back() {
        corrupt_winner_then_repair(
            "CREATE TRIGGER corrupt_pair AFTER INSERT ON conversations \
             WHEN NEW.placement_kind='direct_pair' BEGIN UPDATE conversations SET \
             endpoint_b_id='' WHERE id=NEW.id; END",
            "endpoint_b_id",
        )
        .await;
    }
}
