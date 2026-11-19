//! Human task history: the task's lifecycle system messages behind the by-id gate.

use silverhand_backend_core::conversation_messaging::messaging::MessageId;
use silverhand_backend_core::conversation_messaging::messaging::task::{
    TaskHistoryEvent, TaskHistoryRead, TaskId, TaskReadActor, TaskReadAddress, TaskReadRequest,
    authorize_task_point_on,
};
use silverhand_backend_core::foundation::identity::UserId;
use silverhand_backend_core::foundation::persistence::PersistenceFailure;
use sqlx::Row as _;

use super::super::FROM_KIND_SYSTEM;
use super::point_detail::{SnapshotAuthority, fetch_point_on};
use super::{SqliteTaskStore, corrupt, driver};

// Task events are only ever appended to the task's own conversation.
const SELECT_TASK_EVENTS: &str = "SELECT id, created_at, metadata FROM messages \
     WHERE conversation_id = ? AND from_kind = ? \
     AND json_valid(metadata) \
     AND json_extract(metadata, '$.task_id') = ? \
     AND json_extract(metadata, '$.subtype') IN \
         ('task_create','task_claim','task_unclaim','task_status','task_edit') \
     ORDER BY seq";

impl TaskHistoryRead for SqliteTaskStore {
    async fn read_authorized_history(
        &self,
        viewer: UserId,
        task: TaskId,
    ) -> Result<Option<Vec<TaskHistoryEvent>>, PersistenceFailure> {
        let mut connection = self.pool.begin().await.map_err(driver)?;
        let result = async {
            let request = TaskReadRequest {
                address: TaskReadAddress::ById(task.clone()),
                actor: TaskReadActor::User(viewer),
            };
            let Some(candidate) = fetch_point_on(&mut connection, &request.address).await? else {
                return Ok(None);
            };
            let conversation = candidate.task().conversation_id().clone();
            let mut authority = SnapshotAuthority {
                connection: &mut connection,
            };
            if authorize_task_point_on(&request, candidate, &mut authority)
                .await?
                .is_none()
            {
                return Ok(None);
            }
            let rows = sqlx::query(SELECT_TASK_EVENTS)
                .bind(conversation.as_str())
                .bind(FROM_KIND_SYSTEM)
                .bind(task.as_str())
                .fetch_all(&mut *connection)
                .await
                .map_err(driver)?;
            rows.iter()
                .map(|row| {
                    let id: String = row.try_get("id").map_err(driver)?;
                    let metadata: String = row.try_get("metadata").map_err(driver)?;
                    Ok(TaskHistoryEvent {
                        id: MessageId::parse(id).map_err(|_| corrupt("messages.id"))?,
                        created_at: row.try_get("created_at").map_err(driver)?,
                        metadata: serde_json::from_str(&metadata)
                            .map_err(|_| corrupt("messages.metadata"))?,
                    })
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Some)
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
