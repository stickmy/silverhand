import { taskWorkspaceId } from "../model/task-place.js";
import {
  useMutation,
  useQuery,
  useQueryClient,
  type InfiniteData,
  type QueryClient,
} from "@tanstack/react-query";
import { useEffect } from "react";
import { toast } from "sonner";
import { t } from "@/lib/i18n";
import {
  type CreatedTaskIdentity,
  type HumanCreateTaskInput,
  type HumanCreateTaskResult,
  type TaskBoardColumnDto,
  type TaskChangedEvent,
  type TaskDetailDto,
  type TaskEditResult,
  type TaskStatus,
  type TaskSummaryDto,
} from "@silverhand/shared";
import { api, ApiError, toPathSegment } from "@/lib/api";
import { isCurrentViewerScope, onWsFrame, subscribeViewerScope } from "@/lib/ws-client";
import { classifyTasksCacheKey } from "../model/board-query";
import {
  shouldRefreshTaskDetail,
  shouldRetryTaskDetail,
  taskDetailKey,
  type TaskDetailFrame,
} from "../model/task-detail";
import {
  beginOptimisticStatusWrite,
  closeOptimisticStatusWrite,
  invalidateTaskPointCaches,
  reconcileTaskStatusWrite,
  restartRacingTaskReads,
  revertOptimisticStatusWrite,
  type OptimisticStatusWrite,
} from "../model/task-projection";

/** A board column's cached value: pages of rows keyed by opaque cursors. */
type BoardColumnPages = InfiniteData<TaskBoardColumnDto, string | null>;

/**
 * Settle every task projection of ONE channel the viewer lost (#task:1689).
 *
 * Key families and their shapes — the `['tasks', …]` root holds THREE different
 * values, so the walk dispatches on `classifyTasksCacheKey` and never on the
 * data's shape (a `.filter` on a page object is a crash, not a miss):
 *
 *   - `['tasks','board','column', …]` holds infinite PAGES of rows; each page's
 *     `items` is filtered by `channel_id`. The server's board read is
 *     membership-gated by the same `EXISTS conversation_users` predicate, so a
 *     refetch returns those pages minus those rows — the patch reproduces that
 *     shape. Page cursors are positions in the surviving order, so they stay
 *     valid and the window keeps paging.
 *   - `['tasks','board','counts'|'facets', …]` are server `GROUP BY` aggregates.
 *     They cannot be patched (a client `−1` would invent a number), so they are
 *     INVALIDATED — the authority already excludes the lost channel. This is the
 *     one arm that is not present-only: nothing in the cached value says whether
 *     it counted the lost channel, so a re-read is the only honest answer.
 *   - `['task-detail', id]` and `['task-by-number', ws, n]` are single-object
 *     caches for a task the viewer can no longer read; a refetch fails rather
 *     than returning a trimmed body, so the query is REMOVED, not patched.
 *
 * Otherwise PRESENT-ONLY / idempotent: unloaded or non-matching caches are
 * untouched, so an unrelated channel loss never disturbs an open task board.
 */
export function removeChannelFromTasks(qc: QueryClient, channelId: string): void {
  for (const q of qc.getQueryCache().findAll({ queryKey: ["tasks"] })) {
    switch (classifyTasksCacheKey(q.queryKey)) {
      case "board-column": {
        const prev = qc.getQueryData<BoardColumnPages>(q.queryKey);
        if (!prev) break;
        let changed = false;
        const pages = prev.pages.map((page) => {
          const items = page.items.filter((t) => t.place.conversation_id !== channelId);
          if (items.length === page.items.length) return page;
          changed = true;
          return { ...page, items };
        });
        if (!changed) break;
        qc.setQueryData<BoardColumnPages>(q.queryKey, { ...prev, pages });
        break;
      }
      case "board-counts":
      case "board-facets":
        void qc.invalidateQueries({ queryKey: q.queryKey, exact: true });
        break;
      case "unknown":
        break;
    }
  }
  for (const root of [["task-detail"], ["task-by-number"]] as const) {
    for (const q of qc.getQueryCache().findAll({ queryKey: root })) {
      const data = qc.getQueryData<TaskSummaryDto | TaskDetailDto | null>(q.queryKey);
      if (!data || data.place.conversation_id !== channelId) continue;
      qc.removeQueries({ queryKey: q.queryKey, exact: true });
    }
  }
}

/** Which channels of ONE workspace the task caches still hold rows for
 *  (#task:1689). Read off `workspace_id` + `channel_id` on the row: the board's
 *  default variant spans every workspace, so the key alone cannot answer it.
 *  Only the column pages carry rows; the two aggregates carry none. */
export function tasksHeldChannelIds(qc: QueryClient, workspaceId: string): string[] {
  const ids = new Set<string>();
  const hold = (rows: readonly TaskSummaryDto[]) => {
    for (const t of rows) {
      if (taskWorkspaceId(t) === workspaceId) ids.add(t.place.conversation_id);
    }
  };
  for (const q of qc.getQueryCache().findAll({ queryKey: ["tasks"] })) {
    switch (classifyTasksCacheKey(q.queryKey)) {
      case "board-column":
        for (const page of qc.getQueryData<BoardColumnPages>(q.queryKey)?.pages ?? []) {
          hold(page.items);
        }
        break;
      default:
        break;
    }
  }
  for (const root of [["task-detail"], ["task-by-number"]] as const) {
    for (const q of qc.getQueryCache().findAll({ queryKey: root })) {
      const data = qc.getQueryData<TaskSummaryDto | TaskDetailDto | null>(q.queryKey);
      if (data && taskWorkspaceId(data) === workspaceId) ids.add(data.place.conversation_id);
    }
  }
  return [...ids];
}

// ── Mutations (human UI overrides) ─────────────────────────────────

/**
 * Move a task to another status — the human override of the agent workflow.
 *
 * The card moves BEFORE the write is acknowledged (#task:1975). A status is a
 * value the caller chose, so this client already knows what a success looks
 * like; waiting for the round trip plus a re-read only meant the card sat still
 * for the write (7–8 ms locally, 70–200 ms remote) and then jumped. See
 * `taskProjection.ts` for why a status may be assigned locally where a reaction
 * count may not, and for the three guards that make it safe: compare-and-swap
 * rollback, no invented values, bounded authoritative round.
 *
 * NEITHER FACE OF THE COMMIT INVALIDATES THE ROOT any more. This mutation used
 * to invalidate `['tasks']` and `['task-by-number']` wholesale while the same
 * write's WS echo did it again — two refetch owners re-reading every loaded
 * column, every counts scope, every cached chip, and the facets face a status
 * write cannot even change. What is left is: the projections this client holds
 * are PATCHED, and only what a patch cannot settle is re-read (the two `GROUP BY`
 * aggregates, plus any column whose window could not place the row).
 */
export function useUpdateTaskStatus() {
  const qc = useQueryClient();
  return useMutation<
    { success: boolean; reason?: string },
    Error,
    { taskId: string; status: TaskStatus },
    OptimisticStatusWrite
  >({
    mutationFn: async (args: { taskId: string; status: TaskStatus }) => {
      const res = await api<{ success: boolean; reason?: string }>(
        `/api/tasks/${args.taskId}/status`,
        { method: "POST", body: JSON.stringify({ status: args.status }) },
      );
      if (!res.success) throw new Error(res.reason ?? "update failed");
      return res;
    },
    onMutate: (vars) => beginOptimisticStatusWrite(qc, vars),
    onError: (_err, _vars, write) => {
      if (write) {
        revertOptimisticStatusWrite(qc, write);
        // Reconcile on the failure path too: the revert is refused wherever a
        // newer authority has landed, and a refused write (409
        // `already_at_status`) means the server's value was never ours to guess.
        // `points` is what makes the failure path TOTAL — every chip this client
        // holds for the task is settled by one authoritative read, so the cases
        // the local rollback deliberately declines (a superseded write, a
        // baseline no cache carried) cannot leave a pill stuck on a value the
        // server never had.
        reconcileTaskStatusWrite(qc, write.projection, {
          detail: true,
          points: true,
        });
      }
      // The card has just moved back on its own. Four of the five surfaces that
      // drive this mutation render no failure state, so without this the revert
      // reads as the app undoing the user's click for no reason.
      toast.error(t("tasks.couldntUpdateStatus"));
    },
    onSuccess: (_res, _vars, write) => {
      restartRacingTaskReads(qc, write.taskId);
      reconcileTaskStatusWrite(qc, write.projection, {
        detail: write.closedAtUnknown,
      });
    },
    // Closes this write's entry in the per-task local ledger — the fact that
    // decides whether a LATER write still owns the value on screen.
    onSettled: (_res, _err, _vars, write) => {
      if (write) closeOptimisticStatusWrite(write);
    },
  });
}

/**
 * Assign / re-assign / clear a task's assignee — `agentId: null` clears.
 * `POST /api/tasks/:taskId/claim` `{agent_id}` for a named agent,
 * `POST /api/tasks/:taskId/unclaim` for the clear. Backs the detail modal's
 * Assignee row.
 *
 * ONE hook for both because it is ONE product action ("who owns this"), and both
 * arms need the identical three-projection invalidation and the same error exit;
 * two hooks would let those drift.
 *
 * The renderer never predicts what the write does to the STATUS, and never
 * implies one: assigning a `todo` task DISPATCHES it (the server starts it in the
 * same write — `todo + assignee` has no legal exit), reassigning a running or
 * in-review task only changes hands, and clearing the assignee never rewinds the
 * workflow (backend contract in `docs/tasks-design.md` §4.4, shiki 2026-08-06).
 * Either way the surface re-reads and shows what the server settled on —
 * invalidate, never patch.
 *
 * `done` / `closed` refuse BOTH arms (409): assignment on a terminal task is
 * frozen server-side, which is why the surface offers the control only on a
 * non-terminal one. The 409 stays the authority — the read-only gate is
 * experience, not correctness.
 */
export function useAssignTask() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (args: { taskId: string; agentId: string | null }) => {
      const res = await api<{ success: boolean; reason?: string }>(
        args.agentId === null
          ? `/api/tasks/${args.taskId}/unclaim`
          : `/api/tasks/${args.taskId}/claim`,
        {
          method: "POST",
          ...(args.agentId === null ? {} : { body: JSON.stringify({ agent_id: args.agentId }) }),
        },
      );
      if (!res.success) throw new Error(res.reason ?? "assign failed");
      return res;
    },
    onSuccess: (_res, vars) => {
      // The `['tasks']` ROOT, deliberately — this is the one write that has to be
      // that wide, and each of the three reasons is enough on its own: FACETS are
      // the assignee population and this write changes it (`boardFacetsKey` is
      // refreshed by prefix, one entry per container); the DESTINATION column is
      // not this client's to name, because assigning a `todo` task dispatches it
      // in the same write; and a scope that narrows the assignee axis
      // (`filterKeyConstrainsAssignee`) can GAIN the row in a column this client
      // never held it in, which no content scan can locate.
      void qc.invalidateQueries({ queryKey: ["tasks"] });
      void qc.invalidateQueries({
        queryKey: taskDetailKey(vars.taskId),
        exact: true,
      });
      // The chips, by content: an assignee is shown on every inline `#task:<n>`
      // entry, but only the ones HOLDING this task can have changed — the rest
      // are about other tasks (#task:2091). Not predicting the value and not
      // re-reading unrelated entries are separate questions; this write answers
      // the first with "invalidate" and still owes the second a narrow scope.
      invalidateTaskPointCaches(qc, vars.taskId);
    },
  });
}

// ── Detail + edit (human detail modal) ─────────────────────────────

/**
 * Full single-task read for the detail modal — GET /api/tasks/:taskId
 * → { task: TaskDetailDto }. The board list stays title-only; the body,
 * edit-provenance byline, and viewer-resolved `nav` all come from
 * here, fetched on demand when a card is opened. `enabled` gates the
 * fetch so a closed modal issues no request.
 *
 * This hook is also the SOLE realtime owner of the `['task-detail', id]`
 * projection's REFETCH (#task:1715). It used to ride the listener inside
 * `useTasks`, which only worked while a board was mounted — the modal now opens
 * straight from `?task=<task_id>` (a chat chip, a cold deep-link) with no board
 * behind it, so that arrangement lost both remote `task_changed` refresh and
 * `snapshot_required` gap recovery exactly when the modal was the only task
 * surface on screen. Owning the edge here also means one projection has one
 * owner: no second invalidation of the same key racing this one (a duplicate
 * `invalidateQueries` on a query that already has data CANCELS the in-flight
 * refetch and restarts it — `cancelRefetch` defaults to true — so a second
 * writer is a wasted round trip, not a free dedup).
 *
 * The event's SNAPSHOT is applied elsewhere and that is not a second owner:
 * assignment is idempotent and order-free, so the App-level ingestor patches
 * this projection along with every other one it holds and this listener re-reads
 * only what a snapshot cannot settle — an `edit`'s body, or a gap
 * (`shouldRefreshTaskDetail`, #task:1975).
 *
 * Viewer scope is the right carrier: the server fans `task_changed` out to every
 * human member of the task's conversation, and a human's detail read gate IS
 * conversation membership — so anything the modal is allowed to show, the viewer
 * scope delivers.
 */
export function useTaskDetail(taskId: string | null | undefined, opts?: { enabled?: boolean }) {
  const qc = useQueryClient();
  const gate = opts?.enabled ?? true;
  const enabled = gate && typeof taskId === "string" && taskId.length > 0;

  useEffect(() => {
    if (!enabled || typeof taskId !== "string") return;
    const sub = subscribeViewerScope();
    const unsub = onWsFrame((frame) => {
      // The decision itself is `shouldRefreshTaskDetail` (a pure, exhaustive
      // contract): this listener only projects the wire frame onto it, so the
      // scope/kind/id rules are testable without a React tree.
      const projected: TaskDetailFrame =
        frame.type === "snapshot_required"
          ? {
              type: "snapshot_required",
              viewerScope: isCurrentViewerScope(frame.scope),
            }
          : frame.type === "event"
            ? {
                type: "event",
                viewerScope: isCurrentViewerScope(frame.scope),
                kind: frame.kind,
                taskId: (frame.payload as TaskChangedEvent)?.task_id ?? null,
                taskKind: (frame.payload as TaskChangedEvent)?.kind ?? null,
              }
            : { type: "other" };
      if (!shouldRefreshTaskDetail(projected, taskId)) return;
      void qc.invalidateQueries({ queryKey: taskDetailKey(taskId), exact: true });
    });
    return () => {
      sub.unsubscribe();
      unsub();
    };
  }, [enabled, qc, taskId]);

  return useQuery({
    queryKey: taskDetailKey(taskId),
    queryFn: async (): Promise<TaskDetailDto> => {
      // `taskId` reaches here from `?task=<id>` — a URL the user can paste or
      // hand-edit — so it is untrusted input interpolated into a request path.
      // A value that cannot be one path segment is answered LOCALLY as the same
      // authoritative 404 a deleted task gets: it names no readable task, and
      // sending it would let `..` climb out of `/api/tasks/` in main's
      // `new URL(path, base)` (Cindy / Alice review).
      const segment = toPathSegment(taskId as string);
      if (segment === null) {
        throw new ApiError(404, "Not Found", "malformed task pointer");
      }
      const res = await api<{ task: TaskDetailDto }>(`/api/tasks/${segment}`);
      return res.task;
    },
    // 404 is terminal — see `shouldRetryTaskDetail`.
    retry: shouldRetryTaskDetail,
    enabled,
  });
}

/**
 * `task edit` — a plain last-write-wins UPDATE of the task definition
 * (title and/or content). Returns the full, viewer-resolved detail AFTER the
 * edit so an open detail modal updates in place without a refetch. There is
 * no optimistic-concurrency guard: the most recent successful write wins;
 * every typed failure throws (surfaced via `edit.isError`).
 */
export interface EditTaskInput {
  taskId: string;
  /** Only-changed fields — omit to leave unchanged. A legacy null-title
   *  task keeps its fallback title when `title` is omitted (the backend
   *  materializes it independently of a body-only edit). */
  title?: string;
  body?: string;
}

export function useEditTask() {
  const qc = useQueryClient();
  return useMutation<TaskDetailDto, Error, EditTaskInput>({
    mutationFn: async (input): Promise<TaskDetailDto> => {
      const payload: Record<string, unknown> = {};
      if (input.title !== undefined) payload.title = input.title;
      if (input.body !== undefined) payload.body = input.body;
      const res = await api<Extract<TaskEditResult, { success: true }>>(
        `/api/tasks/${input.taskId}/edit`,
        { method: "POST", body: JSON.stringify(payload) },
      );
      return res.task;
    },
    onSuccess: (task) => {
      // In-place cache update from the authoritative post-edit snapshot;
      // title may have changed so refresh the board list + inline sigils.
      // The list root is the wide one on purpose (a retitled row belongs on
      // every loaded column of every scope); the sigils are not — an edit
      // reaches ONE task, so only the entries holding it can have changed
      // (#task:2091).
      qc.setQueryData(taskDetailKey(task.id), task);
      void qc.invalidateQueries({ queryKey: ["tasks"] });
      invalidateTaskPointCaches(qc, task.id);
    },
  });
}

// ── Create (human panel direct-create) ─────────────────────────────

/**
 * `POST /api/tasks` — create a task straight from a task surface, naming the
 * target channel (#task:1723). Shares the agent create's semantics server-side
 * (one `taskService.createTasks` txn) and, like it, the ACK returns ONLY the
 * identity the transaction produced — `{ id, task_number, title }`, never a
 * post-commit detail re-read (which could report a committed write as a failure
 * and drive a duplicate on this sourceless, non-idempotent create; #task:1724).
 * The identity ACK only CONFIRMS the create — the surface does not navigate to
 * the new task; on success the dialog closes and the `['tasks']` invalidation
 * below refreshes the board in place (shiki 2026-07-30).
 *
 * Only `['tasks']` is invalidated: the new row belongs on every board list, and
 * there is no prior detail / by-number projection of a task that did not exist a
 * moment ago. Typed domain rejections arrive as an `ApiError` carrying the
 * `{ success:false, code }` envelope — `createTaskErrorMessage` (in
 * `createTaskError.ts`) turns the `code` discriminant into copy for the dialog.
 */
export function useCreateTask() {
  const qc = useQueryClient();
  return useMutation<CreatedTaskIdentity, Error, HumanCreateTaskInput>({
    mutationFn: async (input): Promise<CreatedTaskIdentity> => {
      const res = await api<Extract<HumanCreateTaskResult, { success: true }>>("/api/tasks", {
        method: "POST",
        body: JSON.stringify(input),
      });
      return res.task;
    },
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["tasks"] });
    },
  });
}
