//! The task point read: `fetch_by_id`, the single-task committed fact keyed by
//! `tasks.id`.
//!
//! It binds its result to the requested id through the primary-key predicate
//! itself, so the decoded id equals the requested id by construction — the
//! load-bearing requested-coordinate seals (a board-scoped list must not leak a
//! row off its board) belong to the board read models, a separate leaf. Both
//! corruption gates run before the fact is returned: the row-local
//! [`decode_row`](super::row::decode_row) then the cross-row
//! [`seal_board_against_conversation`](super::row::seal_board_against_conversation).

use silverhand_backend_core::conversation_messaging::messaging::task::{TaskAggregate, TaskId};
use silverhand_backend_core::foundation::persistence::PersistenceFailure;

use sqlx::Row as _;

use super::row::{SELECT_TASK_ROW, decode_row, seal_board_against_conversation};
use super::{SqliteTaskStore, corrupt, driver};

impl SqliteTaskStore {
    /// The committed task fact for `id`, or `None` when no such row exists. The row
    /// is decoded through the row-local corruption gate and then its board is sealed
    /// against the authoritative conversation placement; either failure is a
    /// safe-500 carrying the offending column, never a partial or wrong-board fact.
    ///
    /// The result is bound to the requested id through the primary-key predicate
    /// itself, so the decoded id equals the requested id by construction — the
    /// load-bearing requested-coordinate seals (a board-scoped list must not leak a
    /// row off its board) belong to the board read models, a separate leaf.
    pub async fn fetch_by_id(
        &self,
        id: &TaskId,
    ) -> Result<Option<TaskAggregate>, PersistenceFailure> {
        let mut builder = sqlx::QueryBuilder::<sqlx::Sqlite>::new(SELECT_TASK_ROW);
        builder.push(" WHERE t.id = ").push_bind(id.as_str());
        let Some(row) = builder
            .build()
            .fetch_optional(&self.pool)
            .await
            .map_err(driver)?
        else {
            return Ok(None);
        };

        let aggregate = decode_row(&row)?;
        seal_board_against_conversation(&row, &aggregate)?;
        Ok(Some(aggregate))
    }

    /// Resolve a task's canonical id from its per-board `(board_key, task_number)`
    /// business key — the coordinate a task-write by-number arm carries. The
    /// `UNIQUE(board_key, task_number)` index makes this a point read: `Some(id)` or
    /// `None`. It returns ONLY the id (not the aggregate), because the by-number arm
    /// re-drives the write against the canonical id and the write UoW re-reads the
    /// decision-relevant columns under its own lock; a full projection here would be
    /// a second read the write does not trust.
    pub async fn resolve_id_by_number(
        &self,
        board_key: &str,
        task_number: i64,
    ) -> Result<Option<TaskId>, PersistenceFailure> {
        let row = sqlx::query("SELECT id FROM tasks WHERE board_key = ? AND task_number = ?")
            .bind(board_key)
            .bind(task_number)
            .fetch_optional(&self.pool)
            .await
            .map_err(driver)?;
        let Some(row) = row else { return Ok(None) };
        let id: String = row.try_get("id").map_err(driver)?;
        Ok(Some(TaskId::parse(id).map_err(|_| corrupt("tasks.id"))?))
    }
}

/// The production binding of the core [`TaskNumberResolver`] port: the by-number →
/// canonical-id read the internal write faces make before their write. The core
/// trait lives in `task::write_door`, so this impl for the store is orphan-rule
/// legal here (exactly like `TaskWriteDoorReads`); it delegates to the inherent
/// `resolve_id_by_number` above.
impl silverhand_backend_core::conversation_messaging::messaging::task::TaskNumberResolver
    for SqliteTaskStore
{
    async fn resolve_id_by_number(
        &self,
        board_key: &str,
        task_number: i64,
    ) -> Result<Option<TaskId>, PersistenceFailure> {
        SqliteTaskStore::resolve_id_by_number(self, board_key, task_number).await
    }
}

#[cfg(test)]
mod tests {
    use super::super::TaskStoreError;
    use super::SqliteTaskStore;
    use crate::current_schema;
    use silverhand_backend_core::conversation_messaging::messaging::task::{
        BoardKey, TaskId, TaskStatus,
    };
    use silverhand_backend_core::foundation::identity::WorkspaceId;
    use silverhand_backend_core::foundation::persistence::PersistenceFailure;
    use sqlx::{Connection, Row, SqliteConnection};
    use tempfile::TempDir;

    /// Seeds two workspaces, one workspace-placed conversation in `ws-1`, and a
    /// clean `todo` task on the `ws:ws-1` board, then opens the store. The `TempDir`
    /// is returned so the caller keeps the database alive.
    async fn store_with_workspace_task() -> (TempDir, std::path::PathBuf, SqliteTaskStore) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tasks.sqlite");
        let mut db = current_schema::create_current_schema(&path).await;
        for statement in [
            "INSERT INTO users (id, name, display_name, created_at, updated_at) \
             VALUES ('user-1', 'user-1', 'User One', 1, 1)",
            "INSERT INTO workspaces (id, name, name_key, owner_agent_id, owner_user_id, \
             created_at, updated_at) VALUES ('ws-1', 'One', 'one', NULL, 'user-1', 1, 1)",
            "INSERT INTO workspaces (id, name, name_key, owner_agent_id, owner_user_id, \
             created_at, updated_at) VALUES ('ws-2', 'Two', 'two', NULL, 'user-1', 1, 1)",
            "INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, \
             title_key, created_at, updated_at) \
             VALUES ('conv-ws', 'ws-1', 'workspace', 'open', 'General', 'general', 1, 1)",
            "INSERT INTO tasks (id, workspace_id, task_number, conversation_id, title, status, \
             creator_kind, created_at, board_key) \
             VALUES ('task-1', 'ws-1', 1, 'conv-ws', 'hello world', 'todo', 'system', 100, \
             'ws:ws-1')",
        ] {
            sqlx::query(statement).execute(&mut db).await.unwrap();
        }
        db.close().await.unwrap();
        let store = SqliteTaskStore::open(&path).await.unwrap();
        (directory, path, store)
    }

    fn task_id(raw: &str) -> TaskId {
        TaskId::parse(raw).unwrap()
    }

    /// The exact column a corruption safe-500 names, recovered through the full
    /// source chain (`PersistenceFailure` -> `TaskStoreError::Corrupt`). Asserting
    /// on it — not just `is_err()` — proves each gate reports its OWN offending
    /// column: a row-local decode failure mislabeled as the cross-row board field
    /// (or either collapsed to a generic reason) would still be `is_err()` but is
    /// caught here.
    fn corrupt_field(failure: &PersistenceFailure) -> &'static str {
        let source =
            std::error::Error::source(failure).expect("a persistence failure carries its source");
        match source.downcast_ref::<TaskStoreError>() {
            Some(TaskStoreError::Corrupt { field }) => field,
            other => panic!("expected a TaskStoreError::Corrupt, got {other:?}"),
        }
    }

    async fn open_db(path: &std::path::Path) -> SqliteConnection {
        SqliteConnection::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_clean_workspace_task_reads_back_as_the_committed_fact() {
        let (_dir, _path, store) = store_with_workspace_task().await;
        let task = store
            .fetch_by_id(&task_id("task-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(task.id().as_str(), "task-1");
        assert_eq!(task.task_number().get(), 1);
        assert_eq!(task.conversation_id().as_str(), "conv-ws");
        assert_eq!(task.title().as_str(), "hello world");
        assert_eq!(task.status(), TaskStatus::Todo);
        assert_eq!(
            task.board(),
            &BoardKey::Workspace(WorkspaceId::parse("ws-1").unwrap())
        );
    }

    #[tokio::test]
    async fn a_missing_task_is_none_not_an_error() {
        let (_dir, _path, store) = store_with_workspace_task().await;
        assert!(store.fetch_by_id(&task_id("nope")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_direct_pair_task_reads_back_on_its_pair_board() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pair.sqlite");
        let mut db = current_schema::create_current_schema(&path).await;
        for statement in [
            "INSERT INTO users (id, name, display_name, created_at, updated_at) \
             VALUES ('user-1', 'user-1', 'One', 1, 1)",
            "INSERT INTO users (id, name, display_name, created_at, updated_at) \
             VALUES ('user-2', 'user-2', 'Two', 2, 2)",
            "INSERT INTO conversations (id, placement_kind, roster_policy, title, \
             endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id, created_at, \
             updated_at) VALUES ('conv-pair', 'direct_pair', 'fixed', '', 'human', \
             'user-1', 'human', 'user-2', 1, 1)",
        ] {
            sqlx::query(statement).execute(&mut db).await.unwrap();
        }
        // The pair_key is a generated column; read it so the board_key the insert
        // trigger demands is derived from the same authority, not hand-typed.
        let pair_key: String = sqlx::query("SELECT pair_key FROM conversations WHERE id = ?")
            .bind("conv-pair")
            .fetch_one(&mut db)
            .await
            .unwrap()
            .get("pair_key");
        sqlx::query(
            "INSERT INTO tasks (id, task_number, conversation_id, title, status, creator_kind, \
             created_at, board_key) \
             VALUES ('task-pair', 1, 'conv-pair', 'pair task', 'todo', 'system', 100, ?)",
        )
        .bind(format!("pair:{pair_key}"))
        .execute(&mut db)
        .await
        .unwrap();
        db.close().await.unwrap();

        let store = SqliteTaskStore::open(&path).await.unwrap();
        let task = store
            .fetch_by_id(&task_id("task-pair"))
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(task.board(), BoardKey::Pair(_)));
        assert!(task.board().workspace_id().is_none());
    }

    #[tokio::test]
    async fn a_row_local_corrupt_task_is_a_safe_500_repaired_by_fixing_its_column() {
        // A non-canonical stored title violates no `tasks` CHECK (there is none for
        // canonicity), so it INSERTs — but a correct writer stores it canonical, so
        // the row-local decode rejects it. Repairing the one column (and nothing
        // about the conversation) makes the SAME id read cleanly, attributing the
        // failure to the row-local gate, not the cross-row seal.
        let (_dir, path, store) = store_with_workspace_task().await;
        let mut db = open_db(&path).await;
        sqlx::query("UPDATE tasks SET title = 'hello   world' WHERE id = 'task-1'")
            .execute(&mut db)
            .await
            .unwrap();
        db.close().await.unwrap();
        assert_eq!(
            corrupt_field(&store.fetch_by_id(&task_id("task-1")).await.unwrap_err()),
            "tasks.title:non_canonical",
        );

        let mut db = open_db(&path).await;
        sqlx::query("UPDATE tasks SET title = 'hello world' WHERE id = 'task-1'")
            .execute(&mut db)
            .await
            .unwrap();
        db.close().await.unwrap();
        assert!(
            store
                .fetch_by_id(&task_id("task-1"))
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn a_task_number_past_the_js_safe_integer_is_a_safe_500_not_an_imprecise_projection() {
        // The `task_number > 0` CHECK admits any large positive, so a value past
        // Number.MAX_SAFE_INTEGER (2^53 - 1) INSERTs below the write path — but the
        // wire returns task_number as a JSON number, which would ROUND it to a
        // different integer than the digits stored. The row-local decode mints
        // through the core TaskNumber invariant, so it rejects the value as
        // corruption rather than projecting a silently-wrong selector. Repairing the
        // one column back into the safe range makes the SAME id read cleanly.
        let (_dir, path, store) = store_with_workspace_task().await;
        let mut db = open_db(&path).await;
        sqlx::query("UPDATE tasks SET task_number = 9007199254740992 WHERE id = 'task-1'")
            .execute(&mut db)
            .await
            .unwrap();
        db.close().await.unwrap();
        assert_eq!(
            corrupt_field(&store.fetch_by_id(&task_id("task-1")).await.unwrap_err()),
            "tasks.task_number",
        );

        let mut db = open_db(&path).await;
        sqlx::query("UPDATE tasks SET task_number = 9007199254740991 WHERE id = 'task-1'")
            .execute(&mut db)
            .await
            .unwrap();
        db.close().await.unwrap();
        assert_eq!(
            store
                .fetch_by_id(&task_id("task-1"))
                .await
                .unwrap()
                .unwrap()
                .task_number()
                .get(),
            9_007_199_254_740_991,
        );
    }

    #[tokio::test]
    async fn a_board_drifted_from_its_conversation_placement_is_a_safe_500() {
        // A board-vs-conversation drift is UNREACHABLE through any product write:
        // the task's board coordinates are frozen at birth, the conversation's
        // placement is likewise trigger-immutable (`conversations_placement_immutable`),
        // and the insert trigger forces agreement when the task is born. This gate
        // exists for the remaining class — a row mutated below the write path (raw
        // SQL, a future migration bug, a dropped trigger) — the same DB-level
        // corruption the row-local decode gates guard, and the one invariant a
        // single-row decode structurally cannot. So the drift is INJECTED by dropping
        // the immutability trigger and moving only the CONVERSATION to `ws-2`: the
        // authoritative board becomes `ws:ws-2` while the row-local decode still
        // passes, so the cross-row seal alone fires. Restoring the conversation
        // (touching no task column) makes the SAME task read cleanly, attributing the
        // failure to the seal, not the row-local gate.
        let (_dir, path, store) = store_with_workspace_task().await;
        let mut db = open_db(&path).await;
        for statement in [
            "DROP TRIGGER conversations_placement_immutable",
            "UPDATE conversations SET workspace_id = 'ws-2' WHERE id = 'conv-ws'",
        ] {
            sqlx::query(statement).execute(&mut db).await.unwrap();
        }
        db.close().await.unwrap();
        assert_eq!(
            corrupt_field(&store.fetch_by_id(&task_id("task-1")).await.unwrap_err()),
            "tasks.board_key:conversation_placement",
        );

        let mut db = open_db(&path).await;
        sqlx::query("UPDATE conversations SET workspace_id = 'ws-1' WHERE id = 'conv-ws'")
            .execute(&mut db)
            .await
            .unwrap();
        db.close().await.unwrap();
        assert!(
            store
                .fetch_by_id(&task_id("task-1"))
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn resolve_id_by_number_finds_the_task_on_its_board() {
        let (_dir, _path, store) = store_with_workspace_task().await;
        // The seeded task is #1 on the `ws:ws-1` board.
        let resolved = store
            .resolve_id_by_number("ws:ws-1", 1)
            .await
            .unwrap()
            .expect("the board's task #1 resolves");
        assert_eq!(resolved.as_str(), "task-1");
    }

    #[tokio::test]
    async fn resolve_id_by_number_is_none_for_an_unallocated_number_or_wrong_board() {
        let (_dir, _path, store) = store_with_workspace_task().await;
        // A number never allocated on the board.
        assert!(
            store
                .resolve_id_by_number("ws:ws-1", 999)
                .await
                .unwrap()
                .is_none()
        );
        // The right number on a DIFFERENT board resolves nothing (the key is
        // per-board, not global).
        assert!(
            store
                .resolve_id_by_number("ws:ws-2", 1)
                .await
                .unwrap()
                .is_none()
        );
    }
}
