//! SQLite joined point/detail candidate fetch.

use silverhand_backend_core::conversation_messaging::messaging::MessageId;
use std::collections::HashSet;

use silverhand_backend_core::conversation_messaging::messaging::task::{
    BoardKey, LastEditor, PairKey, TaskAuthorizedPointRead, TaskConversationBoardRead,
    TaskMessageRef, TaskParticipationReads, TaskPointCandidate, TaskPointFetch, TaskReadAddress,
    TaskReadCapability, TaskReadRequest, ThreadRootId, VisibleTaskPoint, authorize_task_point_on,
};
use silverhand_backend_core::foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId};
use silverhand_backend_core::foundation::persistence::PersistenceFailure;
use sqlx::{QueryBuilder, Row, Sqlite, SqliteConnection};

use super::super::authorities::TaskWriteAuthorityReader;
use super::super::{SELECT_AGENT_PARTICIPATES, SELECT_USER_PARTICIPATES};
use super::row::{
    SELECT_TASK_JOINED, board_key_string, decode_row, decode_sidecars,
    seal_board_against_conversation,
};
use super::{SqliteTaskStore, corrupt, driver};

struct SnapshotAuthority<'a> {
    connection: &'a mut SqliteConnection,
}

#[cfg(test)]
#[path = "../../../../tests/support/point_read_test_hooks.rs"]
mod test_hooks;
#[cfg(test)]
use test_hooks::POINT_READ_ENTERED;

#[cfg(test)]
async fn pause_point_read() {
    if let Ok(entered) = POINT_READ_ENTERED.try_with(Clone::clone) {
        entered.notify_one();
        std::future::pending::<()>().await;
    }
}

impl TaskParticipationReads for SnapshotAuthority<'_> {
    async fn is_agent_participant(
        &mut self,
        conversation: &ConversationId,
        agent: &AgentId,
    ) -> Result<bool, PersistenceFailure> {
        let hit: Option<i64> = sqlx::query_scalar(SELECT_AGENT_PARTICIPATES)
            .bind(conversation.as_str())
            .bind(agent.as_str())
            .bind(agent.as_str())
            .bind(agent.as_str())
            .fetch_optional(&mut *self.connection)
            .await
            .map_err(driver)?;
        Ok(hit.is_some())
    }

    async fn is_user_participant(
        &mut self,
        conversation: &ConversationId,
        user: &UserId,
    ) -> Result<bool, PersistenceFailure> {
        let hit: Option<i64> = sqlx::query_scalar(SELECT_USER_PARTICIPATES)
            .bind(conversation.as_str())
            .bind(user.as_str())
            .bind(user.as_str())
            .bind(user.as_str())
            .bind(user.as_str())
            .fetch_optional(&mut *self.connection)
            .await
            .map_err(driver)?;
        Ok(hit.is_some())
    }

    async fn agent_participating_conversations(
        &mut self,
        agent: &AgentId,
        conversations: &[ConversationId],
    ) -> Result<HashSet<ConversationId>, PersistenceFailure> {
        let mut visible = HashSet::new();
        for conversation in conversations {
            if self.is_agent_participant(conversation, agent).await? {
                visible.insert(conversation.clone());
            }
        }
        Ok(visible)
    }
}

impl TaskReadCapability for SnapshotAuthority<'_> {
    async fn may_read_tasks(
        &mut self,
        agent: &AgentId,
        workspace: &WorkspaceId,
    ) -> Result<bool, PersistenceFailure> {
        TaskWriteAuthorityReader::new(&mut *self.connection)
            .may_read_tasks(workspace, agent)
            .await
    }

    async fn readable_task_workspaces(
        &mut self,
        agent: &AgentId,
        workspaces: &[WorkspaceId],
    ) -> Result<HashSet<WorkspaceId>, PersistenceFailure> {
        let mut visible = HashSet::new();
        for workspace in workspaces {
            if self.may_read_tasks(agent, workspace).await? {
                visible.insert(workspace.clone());
            }
        }
        Ok(visible)
    }
}

impl TaskAuthorizedPointRead for SqliteTaskStore {
    async fn read_authorized_point(
        &self,
        request: TaskReadRequest,
    ) -> Result<Option<VisibleTaskPoint>, PersistenceFailure> {
        // The candidate and its authorization share a snapshot; the typed owner
        // also rolls back when request cancellation skips the explicit close.
        let mut connection = self.pool.begin().await.map_err(driver)?;
        let result = async {
            #[cfg(test)]
            pause_point_read().await;
            let Some(candidate) = fetch_point_on(&mut connection, &request.address).await? else {
                return Ok(None);
            };
            let mut authority = SnapshotAuthority {
                connection: &mut connection,
            };
            authorize_task_point_on(&request, candidate, &mut authority).await
        }
        .await;
        let close = connection.rollback().await.map_err(driver);
        match (result, close) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(value), Ok(_)) => Ok(value),
        }
    }

    async fn read_authorized_point_by_conversation_number(
        &self,
        viewer: UserId,
        conversation: ConversationId,
        number: silverhand_backend_core::conversation_messaging::messaging::task::TaskNumber,
    ) -> Result<Option<VisibleTaskPoint>, PersistenceFailure> {
        let mut connection = self.pool.begin().await.map_err(driver)?;
        let result = async {
            #[cfg(test)]
            pause_point_read().await;
            let Some(board) = conversation_board_on(&mut connection, &conversation).await? else {
                return Ok(None);
            };
            let request = TaskReadRequest {
                address: TaskReadAddress::ByNumber { board, number },
                actor: silverhand_backend_core::conversation_messaging::messaging::task::TaskReadActor::User(viewer),
            };
            let Some(candidate) = fetch_point_on(&mut connection, &request.address).await? else {
                return Ok(None);
            };
            let mut authority = SnapshotAuthority { connection: &mut connection };
            authorize_task_point_on(&request, candidate, &mut authority).await
        }
        .await;
        let close = connection.rollback().await.map_err(driver);
        match (result, close) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(value), Ok(_)) => Ok(value),
        }
    }
}

async fn conversation_board_on(
    connection: &mut SqliteConnection,
    conversation: &ConversationId,
) -> Result<Option<BoardKey>, PersistenceFailure> {
    let row =
        sqlx::query("SELECT placement_kind,workspace_id,pair_key FROM conversations WHERE id=?")
            .bind(conversation.as_str())
            .fetch_optional(connection)
            .await
            .map_err(driver)?;
    row.map(|row| {
        let placement: String = row.try_get("placement_kind").map_err(driver)?;
        match placement.as_str() {
            "workspace" => {
                let id: String = row.try_get("workspace_id").map_err(driver)?;
                Ok(BoardKey::Workspace(
                    WorkspaceId::parse(id).map_err(|_| corrupt("conversations.workspace_id"))?,
                ))
            }
            "direct_pair" => {
                let key: String = row.try_get("pair_key").map_err(driver)?;
                Ok(BoardKey::Pair(
                    PairKey::parse(key).map_err(|_| corrupt("conversations.pair_key"))?,
                ))
            }
            _ => Err(corrupt("conversations.placement_kind")),
        }
    })
    .transpose()
}

impl TaskConversationBoardRead for SqliteTaskStore {
    async fn conversation_board(
        &self,
        conversation: &silverhand_backend_core::foundation::identity::ConversationId,
    ) -> Result<Option<BoardKey>, PersistenceFailure> {
        let mut connection = self.pool.acquire().await.map_err(driver)?;
        conversation_board_on(&mut connection, conversation).await
    }
}

impl TaskPointFetch for SqliteTaskStore {
    async fn fetch_point(
        &self,
        address: &TaskReadAddress,
    ) -> Result<Option<TaskPointCandidate>, PersistenceFailure> {
        let mut connection = self.pool.acquire().await.map_err(driver)?;
        fetch_point_on(&mut connection, address).await
    }
}

pub(crate) async fn fetch_point_on(
    connection: &mut SqliteConnection,
    address: &TaskReadAddress,
) -> Result<Option<TaskPointCandidate>, PersistenceFailure> {
    let mut builder = QueryBuilder::<Sqlite>::new(SELECT_TASK_JOINED);
    match address {
        TaskReadAddress::ById(id) => {
            builder
                .push(" WHERE t.id=")
                .push_bind(id.as_str().to_owned());
        }
        TaskReadAddress::ByNumber { board, number } => {
            builder
                .push(" WHERE t.board_key=")
                .push_bind(board_key_string(board))
                .push(" AND t.task_number=")
                .push_bind(number.get());
        }
    }
    let Some(row) = builder
        .build()
        .fetch_optional(&mut *connection)
        .await
        .map_err(driver)?
    else {
        return Ok(None);
    };
    let task = decode_row(&row)?;
    seal_board_against_conversation(&row, &task)?;
    let display = decode_sidecars(&row)?;
    let editor_name = match task.last_edit().map(|edit| &edit.editor) {
        Some(LastEditor::Agent(id)) => {
            sqlx::query_scalar::<_, String>("SELECT display_name FROM agents WHERE id=?")
                .bind(id.as_str())
                .fetch_optional(&mut *connection)
                .await
                .map_err(driver)?
        }
        Some(LastEditor::User(id)) => {
            sqlx::query_scalar::<_, String>("SELECT display_name FROM users WHERE id=?")
                .bind(id.as_str())
                .fetch_optional(&mut *connection)
                .await
                .map_err(driver)?
        }
        None => None,
    };
    let source_ref = match task.source_message() {
        Some(id) => message_ref(connection, id.as_str()).await?,
        None => None,
    };
    let created_event_ref = match task.created_event_message() {
        Some(id) => message_ref(connection, id.as_str()).await?,
        None => None,
    };
    Ok(Some(TaskPointCandidate::new(
        task,
        display,
        editor_name,
        source_ref,
        created_event_ref,
    )))
}

async fn message_ref(
    connection: &mut SqliteConnection,
    id: &str,
) -> Result<Option<TaskMessageRef>, PersistenceFailure> {
    let row = sqlx::query("SELECT id,conversation_id,thread_root_id FROM messages WHERE id=?")
        .bind(id)
        .fetch_optional(connection)
        .await
        .map_err(driver)?;
    row.map(|row| {
        let id: String = row.try_get("id").map_err(driver)?;
        let conversation: String = row.try_get("conversation_id").map_err(driver)?;
        let thread: Option<String> = row.try_get("thread_root_id").map_err(driver)?;
        Ok(TaskMessageRef {
            message_id: MessageId::parse(id).map_err(|_| corrupt("messages.id"))?,
            conversation_id: silverhand_backend_core::foundation::identity::ConversationId::parse(
                conversation,
            )
            .map_err(|_| corrupt("messages.conversation_id"))?,
            thread_root_id: thread
                .map(ThreadRootId::parse)
                .transpose()
                .map_err(|_| corrupt("messages.thread_root_id"))?,
        })
    })
    .transpose()
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    use silverhand_backend_core::conversation_messaging::messaging::task::{
        TaskId, TaskReadActor, TaskReadRequest,
    };
    use sqlx::Connection;

    #[tokio::test]
    async fn cancelled_point_ports_return_a_clean_single_pool_connection() {
        use silverhand_backend_core::conversation_messaging::messaging::task::TaskNumber;
        use std::{sync::Arc, time::Duration};
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let store = SqliteTaskStore {
            pool: pool.clone().into(),
        };
        for by_number in [false, true] {
            let entered = Arc::new(tokio::sync::Notify::new());
            let call_store = store.clone();
            let signal = entered.clone();
            let task = tokio::spawn(POINT_READ_ENTERED.scope(signal, async move {
                if by_number {
                    call_store
                        .read_authorized_point_by_conversation_number(
                            UserId::parse("u").unwrap(),
                            ConversationId::parse("c").unwrap(),
                            TaskNumber::new(1).unwrap(),
                        )
                        .await
                } else {
                    call_store
                        .read_authorized_point(TaskReadRequest {
                            address: TaskReadAddress::ById(TaskId::parse("t").unwrap()),
                            actor: TaskReadActor::User(UserId::parse("u").unwrap()),
                        })
                        .await
                }
            }));
            tokio::time::timeout(Duration::from_secs(2), entered.notified())
                .await
                .unwrap();
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            // A one-connection pool cannot hide a poisoned connection behind a fresh one.
            let next = tokio::time::timeout(Duration::from_secs(2), pool.begin())
                .await
                .unwrap();
            assert!(next.is_ok(), "cancelled by_number={by_number}: {next:?}");
            next.unwrap().rollback().await.unwrap();
        }
        pool.close().await;
    }

    mod current_schema {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/current_schema_snapshot.rs"
        ));
    }

    #[tokio::test]
    async fn permission_revoke_after_candidate_projection_cannot_splice_the_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tasks.sqlite");
        let mut seed = current_schema::create_current_schema(&path).await;
        for statement in [
            "INSERT INTO users (id,name,display_name,created_at,updated_at) VALUES ('user-1','user','User',1,1)",
            "INSERT INTO workspaces (id,name,name_key,created_at,updated_at) VALUES ('ws-1','Space','space',1,1)",
            "INSERT INTO workspace_users (workspace_id,user_id,role,joined_at) VALUES ('ws-1','user-1','member',1)",
            "INSERT INTO conversations (id,workspace_id,placement_kind,roster_policy,title,title_key,status,created_at,updated_at) VALUES ('conv-1','ws-1','workspace','open','General','general','active',1,1)",
            "INSERT INTO conversation_users (conversation_id,user_id,joined_at) VALUES ('conv-1','user-1',1)",
            "INSERT INTO tasks (id,workspace_id,task_number,conversation_id,title,content,status,creator_kind,creator_user_id,created_at,board_key) VALUES ('task-1','ws-1',1,'conv-1','Private task','secret','todo','user','user-1',1,'ws:ws-1')",
        ] {
            sqlx::query(statement).execute(&mut seed).await.unwrap();
        }
        sqlx::query("PRAGMA journal_mode = WAL")
            .execute(&mut seed)
            .await
            .unwrap();
        seed.close().await.unwrap();

        let mut read = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        sqlx::query("BEGIN").execute(&mut read).await.unwrap();
        let request = TaskReadRequest {
            address: TaskReadAddress::ById(TaskId::parse("task-1").unwrap()),
            actor: TaskReadActor::User(UserId::parse("user-1").unwrap()),
        };
        let candidate = fetch_point_on(&mut read, &request.address)
            .await
            .unwrap()
            .unwrap();

        // Deterministic latch: the sensitive candidate is already projected and
        // the read transaction has established its snapshot. Revoke on another
        // connection before the permission projection continues.
        let mut writer = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        sqlx::query("DELETE FROM workspace_users WHERE workspace_id='ws-1' AND user_id='user-1'")
            .execute(&mut writer)
            .await
            .unwrap();
        writer.close().await.unwrap();

        let mut authority = SnapshotAuthority {
            connection: &mut read,
        };
        let visible = authorize_task_point_on(&request, candidate, &mut authority)
            .await
            .unwrap();
        assert!(
            visible.is_some(),
            "one request stays wholly on the old snapshot"
        );
        sqlx::query("ROLLBACK").execute(&mut read).await.unwrap();
        read.close().await.unwrap();

        let store = SqliteTaskStore::open(&path).await.unwrap();
        assert!(
            store
                .read_authorized_point(request)
                .await
                .unwrap()
                .is_none(),
            "the next request is wholly on the revoked snapshot"
        );
    }
}
