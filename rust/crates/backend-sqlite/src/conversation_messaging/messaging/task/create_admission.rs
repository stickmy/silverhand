//! SQLite facts consumed by the task-create admission policy.
//!
//! This is deliberately read-only: lifecycle/participation/source checks happen
//! before the batch UoW, while the UNIQUE source constraint and destination
//! read-only predicate are replayed inside the reserved writer.

use silverhand_backend_core::conversation_messaging::messaging::task::{
    ExistingSourceTask, SourceMessageId, TaskCreateAdmissionReads, TaskCreateConversationState,
    TaskCreateSourceRef, TaskId, TaskNumber, TaskStatus,
};
use silverhand_backend_core::foundation::identity::{AgentId, ConversationId};
use silverhand_backend_core::foundation::persistence::PersistenceFailure;
use sqlx::Row;

use super::{corrupt, driver};
use crate::conversation_messaging::messaging::MessagingStore;

impl<E> TaskCreateAdmissionReads for MessagingStore<E>
where
    E: Send + Sync,
{
    async fn agent_exists(&self, agent: &AgentId) -> Result<bool, PersistenceFailure> {
        let present: Option<i64> = sqlx::query_scalar("SELECT 1 FROM agents WHERE id = ?")
            .bind(agent.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(driver)?;
        Ok(present.is_some())
    }

    async fn conversation_state(
        &self,
        conversation: &ConversationId,
    ) -> Result<Option<TaskCreateConversationState>, PersistenceFailure> {
        let row = sqlx::query("SELECT archived_at FROM conversations WHERE id = ?")
            .bind(conversation.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(driver)?;
        row.map(|row| {
            let archived: Option<i64> = row.try_get("archived_at").map_err(driver)?;
            Ok::<_, PersistenceFailure>(if archived.is_some() {
                TaskCreateConversationState::Archived
            } else {
                TaskCreateConversationState::Open
            })
        })
        .transpose()
    }

    async fn source_message(
        &self,
        source: &SourceMessageId,
    ) -> Result<Option<TaskCreateSourceRef>, PersistenceFailure> {
        let row = sqlx::query("SELECT id, conversation_id FROM messages WHERE id = ?")
            .bind(source.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(driver)?;
        row.map(|row| {
            let id: String = row.try_get("id").map_err(driver)?;
            let conversation: String = row.try_get("conversation_id").map_err(driver)?;
            Ok(TaskCreateSourceRef {
                id: SourceMessageId::parse(id).map_err(|_| corrupt("messages.id"))?,
                conversation_id: ConversationId::parse(conversation)
                    .map_err(|_| corrupt("messages.conversation_id"))?,
            })
        })
        .transpose()
    }

    async fn task_for_source(
        &self,
        source: &SourceMessageId,
    ) -> Result<Option<ExistingSourceTask>, PersistenceFailure> {
        let row =
            sqlx::query("SELECT id, task_number, status FROM tasks WHERE source_message_id = ?")
                .bind(source.as_str())
                .fetch_optional(&self.pool)
                .await
                .map_err(driver)?;
        row.map(|row| {
            let id: String = row.try_get("id").map_err(driver)?;
            let number: i64 = row.try_get("task_number").map_err(driver)?;
            let status: String = row.try_get("status").map_err(driver)?;
            Ok(ExistingSourceTask {
                task_id: TaskId::parse(id).map_err(|_| corrupt("tasks.id"))?,
                task_number: TaskNumber::new(number).map_err(|_| corrupt("tasks.task_number"))?,
                status: TaskStatus::parse(&status).map_err(|_| corrupt("tasks.status"))?,
            })
        })
        .transpose()
    }
}
