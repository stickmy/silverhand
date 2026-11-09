//! The board-list candidate fetch: the plural sibling of the point read.
//!
//! `fetch_list_candidates` returns the DECODED candidate SET a
//! [`list_visible_tasks`](silverhand_backend_core::conversation_messaging::messaging::task::list_visible_tasks)
//! call trims — every `tasks` row that matches the request's COORDINATE filters,
//! strict-decoded through the same two gates the point read uses ([`decode_row`] then
//! [`seal_board_against_conversation`]) and paired with the display sidecars the SAME
//! joined select ([`SELECT_TASK_JOINED`]) read for it ([`decode_sidecars`]). It
//! deliberately carries three properties the TS `SqliteTaskRepo.list` folded into one SQL
//! statement but the Rust cutover split out:
//!
//!  - **No participation predicate.** "Which conversations this viewer participates in"
//!    is the per-row gate the core [`list_visible_tasks`] settles through the
//!    `TaskParticipationReads` port, keyed by each row's OWN conversation — never
//!    re-encoded as list SQL here. So this fetch applies only the COORDINATE filters (the
//!    board narrowing and the shared `conversation`/`status`/`task_number`/`assignee`
//!    filters), and the candidate set is a superset of what the viewer will actually see.
//!  - **No `ORDER BY`.** The per-arm total order (`created_at DESC` for a multi-board
//!    agent union, else `task_number DESC`, both with the `id DESC` tiebreak) is the
//!    core's, applied after the fetch — so the candidates may come back in any order.
//!  - **No `LIMIT`.** A SQL limit taken before participation would miscount (it would
//!    keep rows the per-row gate then drops, silently shortening the page). The limit
//!    counts VISIBLE rows and is applied in the core.
//!
//! The board COORDINATE is a `WHERE board_key IN (...)` narrowing for efficiency ONLY:
//! the core re-binds each surviving row's own decoded board against the authorized set, so
//! a stale or hand-mutated `board_key` can never leak a row off its board even though this
//! predicate trusted it. There is no human/workspace arm — the unbounded human list was
//! retired (the human board is a separate cursor read model), so a list is always an
//! agent's authorized board set (the server projects it from the opaque
//! `AuthorizedTaskBoards` witness). An empty board set reads as ZERO rows — never a
//! fallback to an ambient board, and never the invalid `IN ()` — matching the TS arm's
//! early `return []`.

use silverhand_backend_core::conversation_messaging::messaging::task::{
    TaskCandidateFetch, TaskCandidateQuery, TaskListCandidate,
};
use silverhand_backend_core::foundation::persistence::PersistenceFailure;
use sqlx::{QueryBuilder, Sqlite};

use super::row::{
    SELECT_TASK_JOINED, board_key_string, decode_row, decode_sidecars,
    seal_board_against_conversation,
};
use super::{SqliteTaskStore, driver};

impl TaskCandidateFetch for SqliteTaskStore {
    /// The DECODED candidate set for a board list: every `tasks` row matching the
    /// request's coordinate filters, each strict-decoded through the row-local gate and
    /// the cross-row board seal and paired with the display sidecars its joined row read,
    /// in NO particular order. Feeds
    /// [`list_visible_tasks`](silverhand_backend_core::conversation_messaging::messaging::task::list_visible_tasks),
    /// which sorts, participation-gates, and trims. A corrupt row is a safe-500 for the
    /// WHOLE fetch (never a silently-dropped candidate); an empty board set is zero rows
    /// without touching the database.
    async fn fetch_list_candidates(
        &self,
        query: &TaskCandidateQuery,
    ) -> Result<Vec<TaskListCandidate>, PersistenceFailure> {
        // An empty board set is zero rows — short-circuit before building an invalid
        // `board_key IN ()`, matching the TS arm's early `return []`.
        if query.boards.is_empty() {
            return Ok(Vec::new());
        }

        // Built with `QueryBuilder` so the variable-length `IN` lists are parameterized by
        // construction: every dynamic value is a `push_bind`, so there is no interpolated
        // user data and no injection surface — only the static SELECT prefix and the fixed
        // clause fragments are raw SQL.
        let mut builder = QueryBuilder::<Sqlite>::new(SELECT_TASK_JOINED);
        let mut first = true;

        // Non-empty by construction (the empty set short-circuited above).
        conjunction(&mut builder, &mut first);
        builder.push("t.board_key IN (");
        let mut list = builder.separated(", ");
        for board in &query.boards {
            list.push_bind(board_key_string(board));
        }
        list.push_unseparated(")");

        if let Some(conversation) = &query.conversation {
            conjunction(&mut builder, &mut first);
            builder
                .push("t.conversation_id = ")
                .push_bind(conversation.as_str().to_owned());
        }

        if !query.statuses.is_empty() {
            conjunction(&mut builder, &mut first);
            builder.push("t.status IN (");
            let mut list = builder.separated(", ");
            for status in &query.statuses {
                list.push_bind(status.as_str().to_owned());
            }
            list.push_unseparated(")");
        }

        if !query.task_numbers.is_empty() {
            conjunction(&mut builder, &mut first);
            builder.push("t.task_number IN (");
            let mut list = builder.separated(", ");
            for number in &query.task_numbers {
                list.push_bind(number.get());
            }
            list.push_unseparated(")");
        }

        if let Some(assignee) = &query.assignee_agent {
            conjunction(&mut builder, &mut first);
            builder
                .push("t.assignee_agent_id = ")
                .push_bind(assignee.as_str().to_owned());
        }

        let rows = builder
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(driver)?;

        // Strict-decode EVERY fetched row before returning — no row is dropped on a
        // decode failure, so a corrupt candidate is a safe-500 rather than a silent hole
        // the trim would hide. The sidecars ride along untouched for the projector.
        let mut candidates = Vec::with_capacity(rows.len());
        for row in &rows {
            let aggregate = decode_row(row)?;
            seal_board_against_conversation(row, &aggregate)?;
            let display = decode_sidecars(row)?;
            candidates.push(TaskListCandidate::new(aggregate, display));
        }
        Ok(candidates)
    }
}

/// Open the next `WHERE` conjunct: emit `WHERE` for the first clause, ` AND ` for each
/// later one, so the caller pushes only the predicate itself.
fn conjunction(builder: &mut QueryBuilder<Sqlite>, first: &mut bool) {
    builder.push(if *first { " WHERE " } else { " AND " });
    *first = false;
}

#[cfg(test)]
mod tests {
    use super::super::TaskStoreError;
    use super::*;
    use crate::current_schema;
    use silverhand_backend_core::conversation_messaging::messaging::task::{
        BoardKey, TaskCandidateFetch, TaskListCandidate, TaskNumber, TaskStatus,
    };
    use silverhand_backend_core::foundation::identity::{AgentId, ConversationId, WorkspaceId};
    use silverhand_backend_core::foundation::persistence::PersistenceFailure;
    use sqlx::{Connection, Row};
    use tempfile::TempDir;

    /// Seeds a fixed scenario: two workspace boards (`ws:ws-1`, `ws:ws-2`) and one
    /// direct-pair board, four clean tasks — two on `ws:ws-1` (in `conv-a` #1 `todo`, and
    /// `conv-b` #2 `in_progress` assigned to `agent-1`), one on `ws:ws-2` (`conv-c` #1
    /// `todo`), and one on the pair board (`conv-d` #1 `todo`, created by `agent-1`). No
    /// roster rows are seeded: participation is the core's per-row gate, so a fetch returns
    /// candidates regardless of membership. Returns the pair board key so the pair-only
    /// tests can address it.
    async fn seed() -> (TempDir, SqliteTaskStore, BoardKey) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("board-list.sqlite");
        let mut db = current_schema::create_current_schema(&path).await;
        for statement in [
            "INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1', 'host', 'local', 0)",
            "INSERT INTO users (id, name, display_name, created_at, updated_at) \
             VALUES ('user-1', 'user-1', 'User One', 1, 1)",
            seed_agent!(
                id = "agent-1",
                name = "lio",
                host_id = "host-1",
                created_at = 0,
                updated_at = 0,
                display_name = "Lio",
                created_by_user_id = "user-1",
            ),
            "INSERT INTO workspaces (id, name, name_key, owner_agent_id, owner_user_id, \
             created_at, updated_at) VALUES ('ws-1', 'One', 'one', NULL, 'user-1', 1, 1)",
            "INSERT INTO workspaces (id, name, name_key, owner_agent_id, owner_user_id, \
             created_at, updated_at) VALUES ('ws-2', 'Two', 'two', NULL, 'user-1', 1, 1)",
            seed_channel!(
                id = "conv-a",
                workspace_id = "ws-1",
                title = "A",
                title_key = "a"
            ),
            seed_channel!(
                id = "conv-b",
                workspace_id = "ws-1",
                title = "B",
                title_key = "b"
            ),
            seed_channel!(
                id = "conv-c",
                workspace_id = "ws-2",
                title = "C",
                title_key = "c"
            ),
            seed_direct_pair!(
                id = "conv-d",
                endpoint_a_kind = "agent",
                endpoint_a_id = "agent-1",
                endpoint_b_kind = "human",
                endpoint_b_id = "user-1",
            ),
            "INSERT INTO tasks (id, workspace_id, task_number, conversation_id, title, status, \
             creator_kind, created_at, board_key) \
             VALUES ('task-1', 'ws-1', 1, 'conv-a', 'first', 'todo', 'system', 100, 'ws:ws-1')",
            "INSERT INTO tasks (id, workspace_id, task_number, conversation_id, title, status, \
             creator_kind, created_at, board_key, assignee_agent_id) \
             VALUES ('task-2', 'ws-1', 2, 'conv-b', 'second', 'in_progress', 'system', 200, \
             'ws:ws-1', 'agent-1')",
            "INSERT INTO tasks (id, workspace_id, task_number, conversation_id, title, status, \
             creator_kind, created_at, board_key) \
             VALUES ('task-3', 'ws-2', 1, 'conv-c', 'third', 'todo', 'system', 300, 'ws:ws-2')",
        ] {
            sqlx::query(statement).execute(&mut db).await.unwrap();
        }
        // The pair_key is a generated column; read it so the board_key the insert trigger
        // demands is derived from the same authority, not hand-typed.
        let pair_key: String = sqlx::query("SELECT pair_key FROM conversations WHERE id = ?")
            .bind("conv-d")
            .fetch_one(&mut db)
            .await
            .unwrap()
            .get("pair_key");
        sqlx::query(
            "INSERT INTO tasks (id, task_number, conversation_id, title, status, creator_kind, \
             creator_agent_id, created_at, board_key) \
             VALUES ('task-4', 1, 'conv-d', 'fourth', 'todo', 'agent', 'agent-1', 400, ?)",
        )
        .bind(format!("pair:{pair_key}"))
        .execute(&mut db)
        .await
        .unwrap();
        db.close().await.unwrap();
        let store = SqliteTaskStore::open(&path).await.unwrap();
        let pair_board = BoardKey::Pair(
            silverhand_backend_core::conversation_messaging::messaging::task::PairKey::parse(
                &pair_key,
            )
            .unwrap(),
        );
        (directory, store, pair_board)
    }

    /// A query with every filter absent — the caller overrides only the axis under test.
    fn query(boards: Vec<BoardKey>) -> TaskCandidateQuery {
        TaskCandidateQuery {
            boards,
            conversation: None,
            statuses: Vec::new(),
            task_numbers: Vec::new(),
            assignee_agent: None,
        }
    }

    fn workspace(raw: &str) -> WorkspaceId {
        WorkspaceId::parse(raw).unwrap()
    }

    fn ws_board(raw: &str) -> BoardKey {
        BoardKey::Workspace(workspace(raw))
    }

    /// The two workspace boards — the coordinate the filter tests hold fixed so the axis
    /// under test is the only thing narrowing (the human "no board" arm was retired).
    fn both_workspaces() -> Vec<BoardKey> {
        vec![ws_board("ws-1"), ws_board("ws-2")]
    }

    /// The candidate ids SORTED — the fetch returns them in no defined order (the core
    /// sorts), so every assertion compares against a sorted expectation.
    fn sorted_ids(candidates: &[TaskListCandidate]) -> Vec<String> {
        let mut ids: Vec<String> = candidates
            .iter()
            .map(|candidate| candidate.task().id().as_str().to_owned())
            .collect();
        ids.sort();
        ids
    }

    /// The one candidate with the given id — the fetch is unordered, so a sidecar
    /// assertion picks its row out by id.
    fn candidate<'a>(candidates: &'a [TaskListCandidate], id: &str) -> &'a TaskListCandidate {
        candidates
            .iter()
            .find(|candidate| candidate.task().id().as_str() == id)
            .expect("the candidate under test is in the fetched set")
    }

    #[tokio::test]
    async fn an_agent_board_set_fetches_only_the_listed_boards() {
        let (_dir, store, _pair) = seed().await;
        let candidates = store
            .fetch_list_candidates(&query(vec![ws_board("ws-1")]))
            .await
            .unwrap();
        assert_eq!(sorted_ids(&candidates), vec!["task-1", "task-2"]);
    }

    #[tokio::test]
    async fn an_empty_agent_board_set_is_zero_rows() {
        let (_dir, store, _pair) = seed().await;
        let candidates = store
            .fetch_list_candidates(&query(Vec::new()))
            .await
            .unwrap();
        assert!(candidates.is_empty());
    }

    #[tokio::test]
    async fn an_agent_union_over_multiple_boards_returns_every_board() {
        // No roster is seeded, so every listed board's task is a candidate regardless of
        // membership — the participation gate is the core's, applied after the fetch.
        let (_dir, store, pair) = seed().await;
        let candidates = store
            .fetch_list_candidates(&query(vec![ws_board("ws-1"), ws_board("ws-2"), pair]))
            .await
            .unwrap();
        assert_eq!(
            sorted_ids(&candidates),
            vec!["task-1", "task-2", "task-3", "task-4"]
        );
    }

    #[tokio::test]
    async fn the_status_filter_narrows_to_the_given_statuses() {
        let (_dir, store, _pair) = seed().await;
        let mut request = query(both_workspaces());
        request.statuses = vec![TaskStatus::Todo];
        let candidates = store.fetch_list_candidates(&request).await.unwrap();
        assert_eq!(sorted_ids(&candidates), vec!["task-1", "task-3"]);
    }

    #[tokio::test]
    async fn the_task_number_filter_narrows_across_boards() {
        // #1 exists on both workspace boards; the number filter is board-agnostic within
        // the coordinate set, so both #1 rows match.
        let (_dir, store, _pair) = seed().await;
        let mut request = query(both_workspaces());
        request.task_numbers = vec![TaskNumber::new(1).unwrap()];
        let candidates = store.fetch_list_candidates(&request).await.unwrap();
        assert_eq!(sorted_ids(&candidates), vec!["task-1", "task-3"]);
    }

    #[tokio::test]
    async fn the_conversation_filter_narrows_to_one_conversation() {
        let (_dir, store, _pair) = seed().await;
        let mut request = query(both_workspaces());
        request.conversation = Some(ConversationId::parse("conv-b").unwrap());
        let candidates = store.fetch_list_candidates(&request).await.unwrap();
        assert_eq!(sorted_ids(&candidates), vec!["task-2"]);
    }

    #[tokio::test]
    async fn the_assignee_agent_filter_narrows_to_one_assignee() {
        let (_dir, store, _pair) = seed().await;
        let mut request = query(both_workspaces());
        request.assignee_agent = Some(AgentId::parse("agent-1").unwrap());
        let candidates = store.fetch_list_candidates(&request).await.unwrap();
        assert_eq!(sorted_ids(&candidates), vec!["task-2"]);
    }

    #[tokio::test]
    async fn coordinate_filters_compose_as_a_conjunction() {
        let (_dir, store, _pair) = seed().await;
        let mut request = query(vec![ws_board("ws-1")]);
        request.statuses = vec![TaskStatus::Todo];
        let candidates = store.fetch_list_candidates(&request).await.unwrap();
        assert_eq!(sorted_ids(&candidates), vec!["task-1"]);
    }

    #[tokio::test]
    async fn a_workspace_candidate_carries_its_channel_name_and_actor_names() {
        // A workspace board projects its conversation title as `channel_name`, and the
        // assignee's joined display name rides along; the assignee id itself is on the
        // aggregate, so the sidecar is purely the display name.
        let (_dir, store, _pair) = seed().await;
        let candidates = store
            .fetch_list_candidates(&query(vec![ws_board("ws-1")]))
            .await
            .unwrap();
        let task_2 = candidate(&candidates, "task-2");
        assert_eq!(task_2.display().channel_name.as_deref(), Some("B"));
        assert_eq!(task_2.display().assignee_agent_name.as_deref(), Some("Lio"));
        assert_eq!(task_2.display().assignee_user_name, None);
    }

    #[tokio::test]
    async fn a_direct_pair_candidate_projects_no_channel_name() {
        // `channel_name` is workspace-arm-only by construction (the `CASE` in
        // `SELECT_TASK_JOINED`): a direct-pair board's sentinel storage title never enters
        // this read model, so its `channel_name` is `None` even though the conversation
        // has a (sentinel) title column. Its creator's joined name still rides along.
        let (_dir, store, pair) = seed().await;
        let candidates = store
            .fetch_list_candidates(&query(vec![pair]))
            .await
            .unwrap();
        let task_4 = candidate(&candidates, "task-4");
        assert_eq!(task_4.display().channel_name, None);
        assert_eq!(task_4.display().creator_agent_name.as_deref(), Some("Lio"));
    }

    /// The exact column a corruption safe-500 names, recovered through the source chain.
    fn corrupt_field(failure: &PersistenceFailure) -> &'static str {
        let source =
            std::error::Error::source(failure).expect("a persistence failure carries its source");
        match source.downcast_ref::<TaskStoreError>() {
            Some(TaskStoreError::Corrupt { field }) => field,
            other => panic!("expected a TaskStoreError::Corrupt, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_corrupt_candidate_is_a_safe_500_for_the_whole_fetch() {
        // A row that fails strict decode is never silently dropped from the candidate
        // set — the whole fetch is a safe-500 BEFORE the core trim, so a corrupt row can
        // never hide behind a limit.
        let (_dir, store, _pair) = seed().await;
        let path = _dir.path().join("board-list.sqlite");
        let mut db = sqlx::SqliteConnection::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        sqlx::query("UPDATE tasks SET title = 'not   canonical' WHERE id = 'task-1'")
            .execute(&mut db)
            .await
            .unwrap();
        db.close().await.unwrap();

        let failure = match store.fetch_list_candidates(&query(both_workspaces())).await {
            Ok(_) => panic!("a corrupt candidate must be a safe-500, not an Ok set"),
            Err(failure) => failure,
        };
        assert_eq!(corrupt_field(&failure), "tasks.title:non_canonical");
    }
}
