//! SQLite snapshot and write units for human thread following.

use crate::conversation_messaging::runtime_delivery::conversation_allows_delivery;
use std::collections::HashSet;

use silverhand_backend_core::{
    conversation_messaging::messaging::{
        PrincipalIdentity, ThreadRootId,
        collaboration::thread_follow::{
            CommittedHumanFollow, CommittedHumanUnfollow, FollowedAt, HumanFollowRefusal,
            HumanUnfollowRefusal, ThreadFollowPort, ThreadFollowerAgent, ThreadFollowerState,
            ThreadFollowerStateRefusal,
        },
        environment::{EnvironmentUnavailable, SendClock},
    },
    foundation::identity::{AgentId, ConversationId, UserId},
    foundation::persistence::PersistenceFailure,
};
use sqlx::{Row, Sqlite, Transaction, sqlite::SqliteRow};
use thiserror::Error;

use crate::conversation_messaging::messaging::MessagingStore;

use super::conversation_participation::{
    ConversationParticipationStoreError, Placement, decode_conversation, participates,
};

#[derive(Clone)]
pub struct SqliteThreadFollow<E> {
    pool: crate::foundation::database::SqliteDatabase,
    environment: E,
}

impl<E: Clone> MessagingStore<E> {
    pub fn thread_follow(&self) -> SqliteThreadFollow<E> {
        SqliteThreadFollow {
            pool: self.pool.clone(),
            environment: self.environment.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ThreadFollowStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("agent follower candidate set could not be encoded")]
    Serialization(#[from] serde_json::Error),
    #[error("conversation access snapshot is malformed")]
    Access(#[from] ConversationParticipationStoreError),
    #[error("stored thread-follow identity is malformed")]
    MalformedIdentity,
    #[error("stored thread follower row has an unknown principal kind or source")]
    MalformedFollower,
    #[error("stored thread follower followed_at is outside its domain")]
    MalformedFollowedAt,
    #[error("stored thread follower set is not in unique total order")]
    MalformedFollowerSet,
    #[error("host clock failed")]
    Environment(#[from] EnvironmentUnavailable),
    #[error("host clock returned a timestamp outside the thread-follow domain")]
    InvalidClock,
}

impl From<ThreadFollowStoreError> for PersistenceFailure {
    fn from(value: ThreadFollowStoreError) -> Self {
        Self::from_source(value)
    }
}

impl<E> ThreadFollowPort for SqliteThreadFollow<E>
where
    E: SendClock + Clone + Send + Sync + 'static,
{
    type Error = PersistenceFailure;

    async fn state(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
        message: &ThreadRootId,
    ) -> Result<Result<ThreadFollowerState, ThreadFollowerStateRefusal>, Self::Error> {
        self.state_inner(viewer, conversation, message)
            .await
            .map_err(PersistenceFailure::from)
    }

    async fn follow(
        &self,
        viewer: UserId,
        conversation: ConversationId,
        message: ThreadRootId,
    ) -> Result<Result<CommittedHumanFollow, HumanFollowRefusal>, Self::Error> {
        self.follow_inner(viewer, conversation, message)
            .await
            .map_err(PersistenceFailure::from)
    }

    async fn unfollow(
        &self,
        viewer: UserId,
        conversation: ConversationId,
        message: ThreadRootId,
    ) -> Result<Result<CommittedHumanUnfollow, HumanUnfollowRefusal>, Self::Error> {
        self.unfollow_inner(viewer, conversation, message)
            .await
            .map_err(PersistenceFailure::from)
    }
}

impl<E> SqliteThreadFollow<E>
where
    E: SendClock + Clone + Send + Sync + 'static,
{
    async fn state_inner(
        &self,
        viewer: &UserId,
        conversation: &ConversationId,
        message: &ThreadRootId,
    ) -> Result<Result<ThreadFollowerState, ThreadFollowerStateRefusal>, ThreadFollowStoreError>
    {
        let mut tx = self.pool.begin_with("BEGIN DEFERRED").await?;
        let placement = match access(&mut tx, viewer, conversation).await? {
            Access::NotFound => {
                tx.commit().await?;
                return Ok(Err(ThreadFollowerStateRefusal::ConversationNotFound));
            }
            Access::Forbidden => {
                tx.commit().await?;
                return Ok(Err(ThreadFollowerStateRefusal::Forbidden));
            }
            Access::Allowed(placement) => placement,
        };
        let Some(root) = resolve_root(&mut tx, conversation, message).await? else {
            tx.commit().await?;
            return Ok(Err(ThreadFollowerStateRefusal::RootNotFound));
        };

        let rows = sqlx::query(
            "SELECT thread_root_id,conversation_id,principal_kind,principal_id,followed_at,source \
             FROM thread_followers WHERE thread_root_id=? \
             ORDER BY followed_at ASC,principal_kind ASC,principal_id ASC",
        )
        .bind(root.id.as_str())
        .fetch_all(&mut *tx)
        .await?;
        // Complete-snapshot safety: no product filtering starts until every
        // selected row has passed the closed persisted-row decoder.
        let decoded = rows
            .iter()
            .map(|row| decode_follower(row, conversation, &root.id))
            .collect::<Result<Vec<_>, _>>()?;
        let live_agents =
            live_participating_agents(&mut tx, conversation, &placement, &decoded).await?;
        let mut viewer_following = false;
        let mut agents = Vec::new();
        for decoded in decoded {
            match decoded.principal {
                FollowerPrincipal::Human(user) => {
                    if &user == viewer {
                        viewer_following = true;
                    }
                }
                FollowerPrincipal::Agent(agent) => {
                    if live_agents.contains(&agent) {
                        agents.push(ThreadFollowerAgent::projected(
                            decoded.conversation,
                            decoded.root,
                            agent,
                            decoded.followed_at,
                        ));
                    }
                }
            }
        }
        let value = ThreadFollowerState::projected(
            viewer.clone(),
            conversation.clone(),
            message.clone(),
            root.conversation,
            root.id,
            viewer_following,
            agents,
        )
        .map_err(|_| ThreadFollowStoreError::MalformedFollowerSet)?;
        tx.commit().await?;
        Ok(Ok(value))
    }

    async fn follow_inner(
        &self,
        viewer: UserId,
        conversation: ConversationId,
        message: ThreadRootId,
    ) -> Result<Result<CommittedHumanFollow, HumanFollowRefusal>, ThreadFollowStoreError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        match access(&mut tx, &viewer, &conversation).await? {
            Access::NotFound => {
                tx.rollback().await?;
                return Ok(Err(HumanFollowRefusal::ConversationNotFound));
            }
            Access::Forbidden => {
                tx.rollback().await?;
                return Ok(Err(HumanFollowRefusal::Forbidden));
            }
            Access::Allowed(_) => {}
        }
        let Some(root) = resolve_root(&mut tx, &conversation, &message).await? else {
            tx.rollback().await?;
            return Ok(Err(HumanFollowRefusal::RootNotFound));
        };
        let user_exists: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=?)")
            .bind(viewer.as_str())
            .fetch_one(&mut *tx)
            .await?;
        if user_exists != 1 {
            tx.rollback().await?;
            return Err(ThreadFollowStoreError::MalformedIdentity);
        }
        let existing = existing_human(&mut tx, &viewer, &conversation, &root.id).await?;
        let changed = if existing.is_some() {
            false
        } else {
            let followed_at = FollowedAt::new(self.environment.now_unix_ms()?)
                .map_err(|_| ThreadFollowStoreError::InvalidClock)?;
            sqlx::query(
                "INSERT INTO thread_followers \
                 (thread_root_id,conversation_id,principal_kind,principal_id,followed_at,source) \
                 VALUES(?,?,'human',?,?,'manual') \
                 ON CONFLICT(thread_root_id,principal_kind,principal_id) DO NOTHING",
            )
            .bind(root.id.as_str())
            .bind(conversation.as_str())
            .bind(viewer.as_str())
            .bind(followed_at.get())
            .execute(&mut *tx)
            .await?
            .rows_affected()
                == 1
        };
        let committed = CommittedHumanFollow::committed(
            viewer,
            conversation,
            message,
            root.conversation,
            root.id,
            changed,
        );
        tx.commit().await?;
        Ok(Ok(committed))
    }

    async fn unfollow_inner(
        &self,
        viewer: UserId,
        conversation: ConversationId,
        message: ThreadRootId,
    ) -> Result<Result<CommittedHumanUnfollow, HumanUnfollowRefusal>, ThreadFollowStoreError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        match access(&mut tx, &viewer, &conversation).await? {
            Access::NotFound => {
                tx.rollback().await?;
                return Ok(Err(HumanUnfollowRefusal::ConversationNotFound));
            }
            Access::Forbidden => {
                tx.rollback().await?;
                return Ok(Err(HumanUnfollowRefusal::Forbidden));
            }
            Access::Allowed(_) => {}
        }
        let Some(root) = resolve_root(&mut tx, &conversation, &message).await? else {
            tx.rollback().await?;
            return Ok(Err(HumanUnfollowRefusal::RootNotFound));
        };
        let user_exists: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=?)")
            .bind(viewer.as_str())
            .fetch_one(&mut *tx)
            .await?;
        if user_exists != 1 {
            tx.rollback().await?;
            return Err(ThreadFollowStoreError::MalformedIdentity);
        }
        let existing = existing_human(&mut tx, &viewer, &conversation, &root.id).await?;
        let changed = if existing.is_some() {
            sqlx::query(
                "DELETE FROM thread_followers WHERE thread_root_id=? \
                 AND principal_kind='human' AND principal_id=?",
            )
            .bind(root.id.as_str())
            .bind(viewer.as_str())
            .execute(&mut *tx)
            .await?
            .rows_affected()
                == 1
        } else {
            false
        };
        let committed = CommittedHumanUnfollow::committed(
            viewer,
            conversation,
            message,
            root.conversation,
            root.id,
            changed,
        );
        tx.commit().await?;
        Ok(Ok(committed))
    }
}

enum Access {
    NotFound,
    Forbidden,
    Allowed(Placement),
}

async fn access(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    conversation: &ConversationId,
) -> Result<Access, ThreadFollowStoreError> {
    let Some(row) = sqlx::query(
        "SELECT id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,\
         endpoint_a_kind,endpoint_a_id,endpoint_a_deleted_at,\
         endpoint_b_kind,endpoint_b_id,endpoint_b_deleted_at \
         FROM conversations WHERE id=?",
    )
    .bind(conversation.as_str())
    .fetch_optional(&mut **tx)
    .await?
    else {
        return Ok(Access::NotFound);
    };
    let (_, placement) = decode_conversation(&row, conversation)?;
    if !participates(tx, viewer, conversation, &placement).await? {
        return Ok(Access::Forbidden);
    }
    Ok(Access::Allowed(placement))
}

async fn resolve_root(
    tx: &mut Transaction<'_, Sqlite>,
    conversation: &ConversationId,
    message: &ThreadRootId,
) -> Result<Option<SelectedRoot>, ThreadFollowStoreError> {
    let Some(row) = sqlx::query(
        "SELECT id,conversation_id,thread_root_id FROM messages WHERE conversation_id=? AND id=?",
    )
    .bind(conversation.as_str())
    .bind(message.as_str())
    .fetch_optional(&mut **tx)
    .await?
    else {
        return Ok(None);
    };
    let id = ThreadRootId::parse(row.try_get::<String, _>("id")?)
        .map_err(|_| ThreadFollowStoreError::MalformedIdentity)?;
    let stored_conversation = ConversationId::parse(row.try_get::<String, _>("conversation_id")?)
        .map_err(|_| ThreadFollowStoreError::MalformedIdentity)?;
    if &stored_conversation != conversation {
        return Err(ThreadFollowStoreError::MalformedIdentity);
    }
    let root = match row.try_get::<Option<String>, _>("thread_root_id")? {
        Some(root) => {
            ThreadRootId::parse(root).map_err(|_| ThreadFollowStoreError::MalformedIdentity)?
        }
        None => id,
    };
    let root_row = sqlx::query(
        "SELECT id,conversation_id,thread_root_id FROM messages WHERE id=? AND conversation_id=?",
    )
    .bind(root.as_str())
    .bind(conversation.as_str())
    .fetch_optional(&mut **tx)
    .await?;
    let Some(root_row) = root_row else {
        return Err(ThreadFollowStoreError::MalformedIdentity);
    };
    let selected_id = ThreadRootId::parse(root_row.try_get::<String, _>("id")?)
        .map_err(|_| ThreadFollowStoreError::MalformedIdentity)?;
    let selected_conversation =
        ConversationId::parse(root_row.try_get::<String, _>("conversation_id")?)
            .map_err(|_| ThreadFollowStoreError::MalformedIdentity)?;
    let selected_parent: Option<String> = root_row.try_get("thread_root_id")?;
    if selected_id != root || &selected_conversation != conversation || selected_parent.is_some() {
        return Err(ThreadFollowStoreError::MalformedIdentity);
    }
    Ok(Some(SelectedRoot {
        conversation: selected_conversation,
        id: selected_id,
    }))
}

struct SelectedRoot {
    conversation: ConversationId,
    id: ThreadRootId,
}

async fn existing_human(
    tx: &mut Transaction<'_, Sqlite>,
    viewer: &UserId,
    conversation: &ConversationId,
    root: &ThreadRootId,
) -> Result<Option<DecodedFollower>, ThreadFollowStoreError> {
    let row = sqlx::query(
        "SELECT thread_root_id,conversation_id,principal_kind,principal_id,followed_at,source \
         FROM thread_followers WHERE thread_root_id=? AND principal_kind='human' AND principal_id=?",
    )
    .bind(root.as_str())
    .bind(viewer.as_str())
    .fetch_optional(&mut **tx)
    .await?;
    row.as_ref()
        .map(|row| decode_follower(row, conversation, root))
        .transpose()
}

struct DecodedFollower {
    conversation: ConversationId,
    root: ThreadRootId,
    principal: FollowerPrincipal,
    followed_at: FollowedAt,
}

enum FollowerPrincipal {
    Human(UserId),
    Agent(AgentId),
}

fn decode_follower(
    row: &SqliteRow,
    conversation: &ConversationId,
    root: &ThreadRootId,
) -> Result<DecodedFollower, ThreadFollowStoreError> {
    let stored_root = ThreadRootId::parse(row.try_get::<String, _>("thread_root_id")?)
        .map_err(|_| ThreadFollowStoreError::MalformedIdentity)?;
    let stored_conversation = ConversationId::parse(row.try_get::<String, _>("conversation_id")?)
        .map_err(|_| ThreadFollowStoreError::MalformedIdentity)?;
    if &stored_root != root || &stored_conversation != conversation {
        return Err(ThreadFollowStoreError::MalformedIdentity);
    }
    let raw_id: String = row.try_get("principal_id")?;
    let principal = match row.try_get::<String, _>("principal_kind")?.as_str() {
        "human" => FollowerPrincipal::Human(
            UserId::parse(raw_id).map_err(|_| ThreadFollowStoreError::MalformedIdentity)?,
        ),
        "agent" => FollowerPrincipal::Agent(
            AgentId::parse(raw_id).map_err(|_| ThreadFollowStoreError::MalformedIdentity)?,
        ),
        _ => return Err(ThreadFollowStoreError::MalformedFollower),
    };
    if !matches!(
        row.try_get::<String, _>("source")?.as_str(),
        "reply" | "mention" | "manual" | "target_author"
    ) {
        return Err(ThreadFollowStoreError::MalformedFollower);
    }
    let followed_at = FollowedAt::new(row.try_get("followed_at")?)
        .map_err(|_| ThreadFollowStoreError::MalformedFollowedAt)?;
    Ok(DecodedFollower {
        conversation: stored_conversation,
        root: stored_root,
        principal,
        followed_at,
    })
}

async fn live_participating_agents(
    tx: &mut Transaction<'_, Sqlite>,
    conversation: &ConversationId,
    placement: &Placement,
    candidates: &[DecodedFollower],
) -> Result<HashSet<AgentId>, ThreadFollowStoreError> {
    // This is deliberately one set-shaped statement per placement arm.  The
    // number of selected follower candidates changes only the JSON parameter,
    // never the number of authority reads performed in this snapshot.
    let candidate_ids = candidates
        .iter()
        .filter_map(|candidate| match &candidate.principal {
            FollowerPrincipal::Agent(agent) => Some(agent.as_str()),
            FollowerPrincipal::Human(_) => None,
        })
        .collect::<Vec<_>>();
    if candidate_ids.is_empty() {
        return Ok(HashSet::new());
    }
    let encoded = serde_json::to_string(&candidate_ids)?;
    let rows = match placement {
        Placement::Workspace(_) => workspace_live_agent_rows(tx, conversation, &encoded).await?,
        Placement::DirectPair(a, b) => {
            let endpoint_ids = [a, b]
                .into_iter()
                .filter_map(|endpoint| match endpoint {
                    PrincipalIdentity::Agent(agent) => Some(agent.as_str()),
                    PrincipalIdentity::Human(_) => None,
                })
                .collect::<Vec<_>>();
            let endpoints = serde_json::to_string(&endpoint_ids)?;
            direct_pair_live_agent_rows(tx, &encoded, &endpoints).await?
        }
    };
    rows.into_iter()
        .map(|row| {
            AgentId::parse(row.try_get::<String, _>("id")?)
                .map_err(|_| ThreadFollowStoreError::MalformedIdentity)
        })
        .collect()
}

async fn workspace_live_agent_rows(
    tx: &mut Transaction<'_, Sqlite>,
    conversation: &ConversationId,
    candidates: &str,
) -> Result<Vec<SqliteRow>, sqlx::Error> {
    sqlx::query(
        "SELECT a.id FROM json_each(?) candidate \
         JOIN agents a ON a.id=candidate.value \
         JOIN conversation_agents ca ON ca.agent_id=a.id AND ca.conversation_id=? \
         ORDER BY a.id ASC",
    )
    .bind(candidates)
    .bind(conversation.as_str())
    .fetch_all(&mut **tx)
    .await
}

async fn direct_pair_live_agent_rows(
    tx: &mut Transaction<'_, Sqlite>,
    candidates: &str,
    endpoints: &str,
) -> Result<Vec<SqliteRow>, sqlx::Error> {
    sqlx::query(
        "SELECT a.id FROM json_each(?) candidate \
         JOIN agents a ON a.id=candidate.value \
         WHERE a.id IN (SELECT value FROM json_each(?)) ORDER BY a.id ASC",
    )
    .bind(candidates)
    .bind(endpoints)
    .fetch_all(&mut **tx)
    .await
}

impl<E> silverhand_backend_core::conversation_messaging::messaging::collaboration::thread_follow::RootMessageLocate
    for SqliteThreadFollow<E>
where
    E: Send + Sync,
{
    async fn locate_root_message(
        &self,
        message_id: &str,
    ) -> Result<
        Option<silverhand_backend_core::conversation_messaging::messaging::collaboration::thread_follow::RootMessageLocation>,
        silverhand_backend_core::foundation::persistence::PersistenceFailure,
    > {
        use silverhand_backend_core::conversation_messaging::messaging::collaboration::thread_follow::RootMessageLocation;
        let Some(row) = sqlx::query(
            "SELECT m.conversation_id AS conversation_id, c.workspace_id AS workspace_id \
               FROM messages m JOIN conversations c ON c.id = m.conversation_id \
              WHERE m.id = ?",
        )
        .bind(message_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(ThreadFollowStoreError::from)?
        else {
            return Ok(None);
        };
        let conversation = ConversationId::parse(
            row.try_get::<String, _>("conversation_id").map_err(ThreadFollowStoreError::from)?,
        )
        .map_err(|_| silverhand_backend_core::foundation::persistence::PersistenceFailure::from_source(
            ThreadFollowStoreError::MalformedIdentity,
        ))?;
        let workspace = match row
            .try_get::<Option<String>, _>("workspace_id")
            .map_err(ThreadFollowStoreError::from)?
        {
            None => None,
            Some(raw) => Some(
                silverhand_backend_core::foundation::identity::WorkspaceId::parse(raw).map_err(|_| {
                    silverhand_backend_core::foundation::persistence::PersistenceFailure::from_source(
                        ThreadFollowStoreError::MalformedIdentity,
                    )
                })?,
            ),
        };
        Ok(Some(RootMessageLocation { conversation, workspace }))
    }
}

impl<E> silverhand_backend_core::conversation_messaging::messaging::collaboration::thread_follow::AgentThreadFollowWrite
    for SqliteThreadFollow<E>
where
    E: silverhand_backend_core::conversation_messaging::messaging::environment::SendClock
        + Clone
        + Send
        + Sync,
{
    async fn follow(
        &self,
        agent: silverhand_backend_core::foundation::identity::AgentId,
        conversation: ConversationId,
        root_message: ThreadRootId,
    ) -> Result<
        Result<
            silverhand_backend_core::conversation_messaging::messaging::collaboration::thread_follow::AgentThreadFollowChange,
            silverhand_backend_core::conversation_messaging::messaging::collaboration::thread_follow::AgentThreadFollowRefusal,
        >,
        silverhand_backend_core::foundation::persistence::PersistenceFailure,
    > {
        use silverhand_backend_core::conversation_messaging::messaging::collaboration::thread_follow::{
            AgentThreadFollowChange, AgentThreadFollowRefusal,
        };
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await.map_err(ThreadFollowStoreError::from)?;
        let Some(root) = resolve_root(&mut tx, &conversation, &root_message).await? else {
            tx.rollback().await.map_err(ThreadFollowStoreError::from)?;
            return Ok(Err(AgentThreadFollowRefusal::RootNotFound));
        };
        // Anti-oracle ORDER: participation BEFORE capability.
        match agent_follow_gates(&mut tx, &conversation, &agent).await? {
            Ok(()) => {}
            Err(refusal) => {
                tx.rollback().await.map_err(ThreadFollowStoreError::from)?;
                return Ok(Err(refusal));
            }
        }
        // Reserved-writer clock, validated through the shared FollowedAt value object
        // (#53): a domain-invalid instant fails closed, no follower/inbox write.
        let now = self.environment.now_unix_ms().map_err(|error| {
            silverhand_backend_core::foundation::persistence::PersistenceFailure::from_source(
                ThreadFollowStoreError::from(error),
            )
        })?;
        let Ok(followed_at_value) =
            silverhand_backend_core::conversation_messaging::messaging::collaboration::thread_follow::FollowedAt::new(now)
        else {
            tx.rollback().await.map_err(ThreadFollowStoreError::from)?;
            return Ok(Err(AgentThreadFollowRefusal::InvalidClock));
        };
        let followed_at = followed_at_value.get();
        let inserted = sqlx::query(
            "INSERT INTO thread_followers \
             (thread_root_id,conversation_id,principal_kind,principal_id,followed_at,source) \
             VALUES(?,?,'agent',?,?,'manual') \
             ON CONFLICT(thread_root_id,principal_kind,principal_id) DO NOTHING",
        )
        .bind(root.id.as_str())
        .bind(conversation.as_str())
        .bind(agent.as_str())
        .bind(followed_at)
        .execute(&mut *tx)
        .await
        .map_err(ThreadFollowStoreError::from)?
        .rows_affected()
            == 1;
        if inserted && conversation_allows_delivery(&mut tx, &conversation)
            .await.map_err(ThreadFollowStoreError::from)? {
            // Backfill the root + the most recent replies as `observe` deliveries.
            // Idempotent on (conversation, agent, message), skips the agent's own
            // messages, ordered by seq.
            sqlx::query(
                "INSERT INTO conversation_inboxes \
                   (conversation_id, agent_id, message_id, delivered_at, created_at, delivery_kind) \
                 SELECT m.conversation_id, ?, m.id, NULL, ?, 'observe' \
                   FROM messages m \
                  WHERE m.conversation_id = ? \
                    AND (m.id = ? OR m.id IN ( \
                         SELECT r.id FROM messages r \
                          WHERE r.conversation_id = m.conversation_id AND r.thread_root_id = ? \
                            AND NOT (r.from_kind = 'agent' AND r.from_agent_id = ?) \
                          ORDER BY r.seq DESC LIMIT ?)) \
                    AND NOT (m.from_kind = 'agent' AND m.from_agent_id = ?) \
                    AND NOT EXISTS (SELECT 1 FROM conversation_inboxes ci \
                                     WHERE ci.conversation_id = m.conversation_id \
                                       AND ci.agent_id = ? AND ci.message_id = m.id) \
                  ORDER BY m.seq ASC",
            )
            .bind(agent.as_str())
            .bind(followed_at)
            .bind(conversation.as_str())
            .bind(root.id.as_str())
            .bind(root.id.as_str())
            .bind(agent.as_str())
            .bind(crate::conversation_messaging::messaging::THREAD_BACKFILL_RECENT_REPLIES)
            .bind(agent.as_str())
            .bind(agent.as_str())
            .execute(&mut *tx)
            .await
            .map_err(ThreadFollowStoreError::from)?;
        }
        tx.commit().await.map_err(ThreadFollowStoreError::from)?;
        Ok(Ok(AgentThreadFollowChange {
            thread_root: root.id,
            changed: inserted,
        }))
    }

    async fn unfollow(
        &self,
        agent: silverhand_backend_core::foundation::identity::AgentId,
        conversation: ConversationId,
        root_message: ThreadRootId,
    ) -> Result<
        Result<
            silverhand_backend_core::conversation_messaging::messaging::collaboration::thread_follow::AgentThreadFollowChange,
            silverhand_backend_core::conversation_messaging::messaging::collaboration::thread_follow::AgentThreadFollowRefusal,
        >,
        silverhand_backend_core::foundation::persistence::PersistenceFailure,
    > {
        use silverhand_backend_core::conversation_messaging::messaging::collaboration::thread_follow::{
            AgentThreadFollowChange, AgentThreadFollowRefusal,
        };
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await.map_err(ThreadFollowStoreError::from)?;
        let Some(root) = resolve_root(&mut tx, &conversation, &root_message).await? else {
            tx.rollback().await.map_err(ThreadFollowStoreError::from)?;
            return Ok(Err(AgentThreadFollowRefusal::RootNotFound));
        };
        // Unfollow re-verifies the same gates in the anti-oracle order:
        // a lock-outside hint is never the authority.
        match agent_follow_gates(&mut tx, &conversation, &agent).await? {
            Ok(()) => {}
            Err(refusal) => {
                tx.rollback().await.map_err(ThreadFollowStoreError::from)?;
                return Ok(Err(refusal));
            }
        }
        // Unfollow only stops FUTURE delivery — the already-delivered backfill is NOT
        // retracted. A single delete; idempotent (`changed=false` when not following).
        let removed = sqlx::query(
            "DELETE FROM thread_followers \
             WHERE thread_root_id=? AND principal_kind='agent' AND principal_id=?",
        )
        .bind(root.id.as_str())
        .bind(agent.as_str())
        .execute(&mut *tx)
        .await
        .map_err(ThreadFollowStoreError::from)?
        .rows_affected()
            == 1;
        tx.commit().await.map_err(ThreadFollowStoreError::from)?;
        Ok(Ok(AgentThreadFollowChange {
            thread_root: root.id,
            changed: removed,
        }))
    }
}

/// The agent follow/unfollow gates fresh-replayed in the reserved writer, in the
/// anti-oracle ORDER: live participation (workspace roster edge OR
/// direct-pair agent endpoint) FIRST, THEN `read_messages` capability on the
/// conversation's workspace (a direct pair has no capability axis — participation is
/// the whole gate). A non-participant is refused before it can probe capability.
async fn agent_follow_gates(
    tx: &mut Transaction<'_, Sqlite>,
    conversation: &ConversationId,
    agent: &silverhand_backend_core::foundation::identity::AgentId,
) -> Result<
    Result<(), silverhand_backend_core::conversation_messaging::messaging::collaboration::thread_follow::AgentThreadFollowRefusal>,
    silverhand_backend_core::foundation::persistence::PersistenceFailure,
>{
    use silverhand_backend_core::conversation_messaging::messaging::collaboration::thread_follow::AgentThreadFollowRefusal;
    // Placement + workspace of the conversation.
    let Some(row) = sqlx::query(
        "SELECT placement_kind, workspace_id, endpoint_a_kind, endpoint_a_id, endpoint_a_deleted_at, \
         endpoint_b_kind, endpoint_b_id, endpoint_b_deleted_at FROM conversations WHERE id = ?",
    )
    .bind(conversation.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(ThreadFollowStoreError::from)?
    else {
        // No such conversation → the agent cannot participate.
        return Ok(Err(AgentThreadFollowRefusal::NotParticipant));
    };
    let placement: String = row
        .try_get("placement_kind")
        .map_err(ThreadFollowStoreError::from)?;
    let participates = if placement == "direct_pair" {
        let mut ok = false;
        for (kind_col, id_col, del_col) in [
            ("endpoint_a_kind", "endpoint_a_id", "endpoint_a_deleted_at"),
            ("endpoint_b_kind", "endpoint_b_id", "endpoint_b_deleted_at"),
        ] {
            let kind: Option<String> = row
                .try_get(kind_col)
                .map_err(ThreadFollowStoreError::from)?;
            let id: Option<String> = row.try_get(id_col).map_err(ThreadFollowStoreError::from)?;
            let deleted: Option<i64> =
                row.try_get(del_col).map_err(ThreadFollowStoreError::from)?;
            if kind.as_deref() == Some("agent")
                && id.as_deref() == Some(agent.as_str())
                && deleted.is_none()
            {
                ok = true;
            }
        }
        ok
    } else {
        // Workspace placement: a live conversation_agents edge to an existing agent.
        let hit: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM conversation_agents ca JOIN agents a ON a.id=ca.agent_id \
             WHERE ca.conversation_id=? AND ca.agent_id=?)",
        )
        .bind(conversation.as_str())
        .bind(agent.as_str())
        .fetch_one(&mut **tx)
        .await
        .map_err(ThreadFollowStoreError::from)?;
        hit == 1
    };
    if !participates {
        return Ok(Err(AgentThreadFollowRefusal::NotParticipant));
    }
    // Capability (workspace placement only).
    let workspace: Option<String> = row
        .try_get("workspace_id")
        .map_err(ThreadFollowStoreError::from)?;
    if let Some(workspace) = workspace {
        let granted = crate::foundation::capability::agent_has_workspace_capability(
            &mut **tx,
            workspace.as_str(),
            agent.as_str(),
            "read_messages",
        )
        .await
        .map_err(ThreadFollowStoreError::from)?;
        if !granted {
            return Ok(Err(AgentThreadFollowRefusal::CapabilityDenied));
        }
    }
    Ok(Ok(()))
}

#[cfg(test)]
mod tests {
    use sqlx::{
        Connection, Executor, SqliteConnection,
        sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    };

    use super::*;

    const FOLLOW_SCHEMA: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-fixtures/migration-baseline/schema-current.sql"
    ));

    // ws channel `chan` (agent-a member+cap; agent-c member no-cap; agent-x nonmember),
    // one root message `m1`.
    const FOLLOW_SEED: &str = "\
INSERT INTO users(id,name,display_name,created_at,updated_at) VALUES('u1','u','U',1,1); \
INSERT INTO hosts(id,name,kind,created_at) VALUES('h1','H','local',1); \
INSERT INTO agents(id,name,runtime,host_id,created_at,updated_at,created_by_user_id) VALUES('agent-a','alice','claude','h1',1,1,'u1'); \
INSERT INTO agents(id,name,runtime,host_id,created_at,updated_at,created_by_user_id) VALUES('agent-c','dave','codex','h1',1,1,'u1'); \
INSERT INTO agents(id,name,runtime,host_id,created_at,updated_at,created_by_user_id) VALUES('agent-x','carol','codex','h1',1,1,'u1'); \
INSERT INTO workspaces(id,name,name_key,owner_agent_id,created_at,updated_at) VALUES('ws','Team','fteam',NULL,1,1); \
INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES('ws','agent-a',1); \
INSERT INTO workspace_members(workspace_id,agent_id,joined_at) VALUES('ws','agent-c',1); \
INSERT INTO workspace_agent_capabilities(workspace_id,agent_id,capability) VALUES('ws','agent-a','read_messages'); \
INSERT INTO conversations(id,workspace_id,placement_kind,roster_policy,title,title_key,status,visibility,created_at,updated_at) VALUES('chan','ws','workspace','open','Chan','fchan','idle','private',1,1); \
INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES('chan','agent-a',1); \
INSERT INTO conversation_agents(conversation_id,agent_id,joined_at) VALUES('chan','agent-c',1); \
INSERT INTO messages(id,conversation_id,from_kind,from_agent_id,content,seq,created_at) VALUES('m1','chan','agent','agent-a','root',1,10); \
INSERT INTO messages(id,conversation_id,from_kind,from_agent_id,content,seq,created_at,thread_root_id) VALUES('m2','chan','agent','agent-c','reply',2,11,'m1');";

    async fn follow_store() -> (
        tempfile::TempDir,
        MessagingStore<crate::conversation_messaging::messaging::HostEnvironment>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-follow.sqlite");
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        db.execute(sqlx::raw_sql(FOLLOW_SCHEMA)).await.unwrap();
        db.execute(sqlx::raw_sql(FOLLOW_SEED)).await.unwrap();
        db.close().await.unwrap();
        let store = MessagingStore::open(&path).await.unwrap();
        (dir, store)
    }
    fn ag(id: &str) -> AgentId {
        AgentId::parse(id).unwrap()
    }
    fn cv(id: &str) -> ConversationId {
        ConversationId::parse(id).unwrap()
    }
    fn rt(id: &str) -> ThreadRootId {
        ThreadRootId::parse(id).unwrap()
    }

    #[tokio::test]
    async fn archived_thread_follow_keeps_follow_fact_without_backfill() {
        use silverhand_backend_core::conversation_messaging::messaging::collaboration::thread_follow::AgentThreadFollowWrite;

        let (dir, store) = follow_store().await;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new().filename(dir.path().join("agent-follow.sqlite")),
            )
            .await
            .unwrap();
        sqlx::query("UPDATE conversations SET archived_at = 100 WHERE id = 'chan'")
            .execute(&pool)
            .await
            .unwrap();

        let change = AgentThreadFollowWrite::follow(
            &store.thread_follow(),
            ag("agent-a"),
            cv("chan"),
            rt("m1"),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(change.changed);
        let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversation_inboxes")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(queued, 0);
        let followers: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM thread_followers WHERE principal_id='agent-a'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(followers, 1);
        AgentThreadFollowWrite::unfollow(
            &store.thread_follow(),
            ag("agent-a"),
            cv("chan"),
            rt("m1"),
        )
        .await
        .unwrap()
        .unwrap();
        sqlx::query("UPDATE conversations SET archived_at=NULL WHERE id='chan'")
            .execute(&pool)
            .await
            .unwrap();
        AgentThreadFollowWrite::follow(&store.thread_follow(), ag("agent-a"), cv("chan"), rt("m1"))
            .await
            .unwrap()
            .unwrap();
        let messages: Vec<String> =
            sqlx::query_scalar("SELECT message_id FROM conversation_inboxes")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(messages, ["m2"]);
    }

    #[tokio::test]
    async fn agent_follow_backfills_only_the_most_recent_replies() {
        use silverhand_backend_core::conversation_messaging::messaging::collaboration::thread_follow::AgentThreadFollowWrite;

        let (dir, store) = follow_store().await;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new().filename(dir.path().join("agent-follow.sqlite")),
            )
            .await
            .unwrap();
        // m3..m16 by agent-c, plus agent-a's own reply at seq 15 that is never queued.
        for seq in 3..=16 {
            let author = if seq == 15 { "agent-a" } else { "agent-c" };
            sqlx::query(
                "INSERT INTO messages(id,conversation_id,from_kind,from_agent_id,content,seq,created_at,thread_root_id) \
                 VALUES(?,'chan','agent',?,'reply',?,?,'m1')",
            )
            .bind(format!("m{seq}"))
            .bind(author)
            .bind(seq)
            .bind(seq + 10)
            .execute(&pool)
            .await
            .unwrap();
        }
        AgentThreadFollowWrite::follow(&store.thread_follow(), ag("agent-a"), cv("chan"), rt("m1"))
            .await
            .unwrap()
            .unwrap();
        let queued: Vec<String> = sqlx::query_scalar(
            "SELECT i.message_id FROM conversation_inboxes i JOIN messages m ON m.id = i.message_id \
             ORDER BY m.seq",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            queued,
            [
                "m6", "m7", "m8", "m9", "m10", "m11", "m12", "m13", "m14", "m16"
            ]
        );
    }

    #[tokio::test]
    async fn agent_follow_backfills_then_revoke_flips_the_next_follow_closed() {
        use silverhand_backend_core::conversation_messaging::messaging::collaboration::thread_follow::{
            AgentThreadFollowWrite, AgentThreadFollowRefusal,
        };
        let (dir, store) = follow_store().await;
        let follow = store.thread_follow();
        // agent-a (member + cap) follows → changed, and backfills the root into inbox.
        let change = AgentThreadFollowWrite::follow(&follow, ag("agent-a"), cv("chan"), rt("m1"))
            .await
            .unwrap()
            .unwrap();
        assert!(change.changed);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(dir.path().join("agent-follow.sqlite"))
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        // The follower row exists after a successful follow.
        let followers_now: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM thread_followers WHERE principal_kind='agent' AND principal_id='agent-a'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(followers_now, 1, "follow inserted the follower row");
        let deliveries: Vec<(String, String)> = sqlx::query_as(
            "SELECT message_id,delivery_kind FROM conversation_inboxes ORDER BY message_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(deliveries, [("m2".to_owned(), "observe".to_owned())]);
        // Revoke read_messages (a competing writer). A brand-new agent-c that IS a
        // member but now needs cap is refused — proving the writer replays capability
        // in-snapshot rather than caching the earlier authorized read (#5).
        sqlx::query("DELETE FROM workspace_agent_capabilities WHERE workspace_id='ws' AND agent_id='agent-a'")
            .execute(&pool).await.unwrap();
        assert!(matches!(
            AgentThreadFollowWrite::follow(&follow, ag("agent-a"), cv("chan"), rt("m1"))
                .await
                .unwrap(),
            Err(AgentThreadFollowRefusal::CapabilityDenied)
        ));
        // agent-c (member, never had cap) also refused — no follower row for it.
        assert!(matches!(
            AgentThreadFollowWrite::follow(&follow, ag("agent-c"), cv("chan"), rt("m1"))
                .await
                .unwrap(),
            Err(AgentThreadFollowRefusal::CapabilityDenied)
        ));
        let c_followers: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM thread_followers WHERE principal_id='agent-c'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            c_followers, 0,
            "a capability-denied follow writes no follower row"
        );
    }

    #[tokio::test]
    async fn agent_follow_refusal_order_and_gates() {
        use silverhand_backend_core::conversation_messaging::messaging::collaboration::thread_follow::{
            AgentThreadFollowWrite, AgentThreadFollowRefusal,
        };
        let (_dir, store) = follow_store().await;
        let follow = store.thread_follow();
        // nonparticipant (agent-x) → NotParticipant BEFORE capability (#8).
        assert!(matches!(
            AgentThreadFollowWrite::follow(&follow, ag("agent-x"), cv("chan"), rt("m1"))
                .await
                .unwrap(),
            Err(AgentThreadFollowRefusal::NotParticipant)
        ));
        // participant without cap (agent-c) → CapabilityDenied.
        assert!(matches!(
            AgentThreadFollowWrite::follow(&follow, ag("agent-c"), cv("chan"), rt("m1"))
                .await
                .unwrap(),
            Err(AgentThreadFollowRefusal::CapabilityDenied)
        ));
        // unknown root → RootNotFound.
        assert!(matches!(
            AgentThreadFollowWrite::follow(&follow, ag("agent-a"), cv("chan"), rt("nope"))
                .await
                .unwrap(),
            Err(AgentThreadFollowRefusal::RootNotFound)
        ));
    }
}
