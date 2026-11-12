import {
  keepPreviousData,
  useInfiniteQuery,
  useQuery,
  useQueryClient,
  type InfiniteData,
} from "@tanstack/react-query";
import { useEffect, useMemo, useState } from "react";
import {
  type TaskBoardColumnDto,
  type TaskBoardCountsDto,
  type TaskBoardFacetsDto,
  type TaskChangedEvent,
  type TaskStatus,
  type TaskSummaryDto,
} from "@silverhand/shared";
import { api } from "@/lib/api";
import {
  isCurrentViewerScope,
  onWsFrame,
  subscribeViewerScope,
  subscribeWsScope,
} from "@/lib/ws-client";
import { useWorkspaces } from "@/features/workspaces/hooks";
import {
  boardAuthority,
  type BoardAuthority,
  type BoardCounts,
  type BoardFacets,
} from "../model/board-authority";
import {
  boardColumnKey,
  boardColumnPath,
  boardCountsKey,
  boardCountsPath,
  boardFacetsKey,
  boardFacetsPath,
  boardFilterKey,
  boardFilterParams,
} from "../model/board-query";
import { columnVisibleCount, type ColumnWindow } from "../model/column-window";
import type { PersonOption, TaskFilters } from "../model/task-filters";
import { applyTaskChangedToProjections, reconcileTaskStatusWrite } from "../model/task-projection";

/**
 * The task board's read layer (#task:1885 against the #task:1884 authority).
 *
 * One column = one `useInfiniteQuery` keyed by `(status, filterKey)`. Per-status
 * queries are what make the window honest: a column's pages advance
 * independently, so one busy column can never eat another's window, and the
 * counts a header shows come from the counts face rather than from however many
 * pages happened to be loaded.
 *
 * WHAT DRIVES FETCHING is the surface's render window, not the query: a column
 * fetches until it has as many pages as the surface wants to render
 * (`window[status]`). The window is surface-local while pages are shared cache
 * (see `columnWindow.ts`), so one surface scrolling to page 5 leaves 5 pages
 * cached without another inheriting them as a render bound — and a collapsed
 * column, pinned to its first page, issues no reads at all.
 *
 * Pages are concatenated in REQUEST order and never re-sorted: the wire contract
 * (`TaskBoardColumnDtoSchema`) delivers `(task_number DESC, id DESC)` with each
 * page continuing strictly after the last, and `task_number` is only monotonic
 * per workspace — a local sort over the loaded prefix would disagree with what
 * the next page continues from. `next_cursor === null` is the ONLY end-of-column
 * signal; a full page can also be the last one.
 */

/** One status column as a surface consumes it. */
export interface BoardColumnRead {
  /** Loaded rows, in delivered order. */
  items: TaskSummaryDto[];
  /** First window still in flight (nothing to render yet). */
  isLoading: boolean;
  isError: boolean;
  /** The column has no further pages (`next_cursor === null`). */
  atEnd: boolean;
  /** Any page of this column is in flight. */
  isFetching: boolean;
  /**
   * WHICH READ these clocks belong to: `(status, filterKey)`. The clocks below
   * are only comparable within one identity — a filter change swaps the query
   * behind this column, and the new one starts at `dataUpdatedAt: 0` /
   * `errorUpdateCount: 0`. Anything treating a clock as evidence must re-baseline
   * when this string changes, or it reads a query swap as a fresh answer
   * (#task:1885, Cindy MUST FIX).
   */
  identity: string;
  /** Latest successful read / failure clocks for THIS column under THIS
   *  identity. */
  dataUpdatedAt: number;
  errorUpdateCount: number;
  /** Re-read this column's loaded pages. Scoped deliberately: a round that needs
   *  this column's answer must not be satisfiable by another column's. */
  refetch: () => void;
}

export type BoardColumns = Readonly<Record<TaskStatus, BoardColumnRead>>;

const EMPTY_ITEMS: TaskSummaryDto[] = [];

// A filter change keeps the previous answer on screen (`keepPreviousData`) so a
// fast response swaps in without a skeleton frame. Past this delay the old
// answer would read as the new filter's result, so the surface drops to its
// loading state instead.
const PLACEHOLDER_GRACE_MS = 300;

function usePlaceholderOverdue(isPlaceholder: boolean): boolean {
  const [overdue, setOverdue] = useState(false);

  useEffect(() => {
    if (!isPlaceholder) {
      setOverdue(false);
      return;
    }
    const timer = setTimeout(() => setOverdue(true), PLACEHOLDER_GRACE_MS);
    return () => clearTimeout(timer);
  }, [isPlaceholder]);

  return isPlaceholder && overdue;
}

function useBoardColumn(
  status: TaskStatus,
  filterKey: string,
  filters: TaskFilters,
  enabled: boolean,
  wantedPages: number,
): BoardColumnRead {
  const query = useInfiniteQuery<
    TaskBoardColumnDto,
    Error,
    InfiniteData<TaskBoardColumnDto, string | null>,
    ReturnType<typeof boardColumnKey>,
    string | null
  >({
    queryKey: boardColumnKey(status, filterKey),
    enabled,
    placeholderData: keepPreviousData,
    initialPageParam: null,
    queryFn: ({ pageParam, signal }) =>
      api<TaskBoardColumnDto>(boardColumnPath(status, boardFilterParams(filters), pageParam), {
        signal,
      }),
    getNextPageParam: (lastPage) => lastPage.next_cursor,
  });

  const loadedPages = query.data?.pages.length ?? 0;
  const { hasNextPage, isFetchingNextPage, fetchNextPage, isPlaceholderData } = query;

  // The window asks, the query answers. Page N+1 is fetched only because some
  // surface wants to render it — there is no prefetch-ahead and no polling, so
  // the read cost of a board nobody is looking at is zero.
  useEffect(() => {
    if (!enabled) return;
    // No first page yet: it is already in flight (or the query is off). Asking
    // for a next page here would fetch the same window twice.
    // Placeholder pages belong to the previous filter; paging from them would
    // race the new key's first read.
    if (loadedPages === 0 || isPlaceholderData) return;
    if (loadedPages >= wantedPages) return;
    if (!hasNextPage || isFetchingNextPage) return;
    void fetchNextPage();
  }, [
    enabled,
    loadedPages,
    isPlaceholderData,
    wantedPages,
    hasNextPage,
    isFetchingNextPage,
    fetchNextPage,
  ]);

  const overdue = usePlaceholderOverdue(isPlaceholderData);
  const items = useMemo(
    () => (overdue ? EMPTY_ITEMS : (query.data?.pages.flatMap((p) => p.items) ?? EMPTY_ITEMS)),
    [overdue, query.data],
  );

  const { isError, isFetching, dataUpdatedAt, errorUpdateCount, refetch } = query;
  const isLoading = query.isLoading || overdue;

  return useMemo(
    () => ({
      items,
      isLoading,
      isError,
      atEnd: !hasNextPage,
      isFetching,
      identity: `${status}|${filterKey}`,
      dataUpdatedAt,
      errorUpdateCount,
      refetch: () => {
        void refetch();
      },
    }),
    [
      items,
      isLoading,
      isError,
      hasNextPage,
      isFetching,
      status,
      filterKey,
      dataUpdatedAt,
      errorUpdateCount,
      refetch,
    ],
  );
}

/**
 * Every column of ONE board surface.
 *
 * `isCollapsed` gates a column's reads: a collapsed column renders no body, so
 * it must not fetch one — that is what keeps a Closed column (the largest one on
 * a mature board) free until the reader opens it. Its header count still comes
 * from the counts face, so collapsing hides rows without hiding the total.
 *
 * The five hooks are written out rather than mapped so hook order is fixed by
 * the code, not by the contents of a status array.
 */
export function useBoardColumns(args: {
  filters: TaskFilters;
  /** Surface-level gate (a hidden surface reads nothing). */
  enabled: boolean;
  window: ColumnWindow;
  isCollapsed: (status: TaskStatus) => boolean;
}): BoardColumns {
  const { filters, enabled, window, isCollapsed } = args;
  const filterKey = boardFilterKey(filters);
  const pages = (status: TaskStatus) => window[status];
  const on = (status: TaskStatus) => enabled && !isCollapsed(status);

  const todo = useBoardColumn("todo", filterKey, filters, on("todo"), pages("todo"));
  const inProgress = useBoardColumn(
    "in_progress",
    filterKey,
    filters,
    on("in_progress"),
    pages("in_progress"),
  );
  const inReview = useBoardColumn(
    "in_review",
    filterKey,
    filters,
    on("in_review"),
    pages("in_review"),
  );
  const done = useBoardColumn("done", filterKey, filters, on("done"), pages("done"));
  const closed = useBoardColumn("closed", filterKey, filters, on("closed"), pages("closed"));

  return useMemo(
    () => ({
      todo,
      in_progress: inProgress,
      in_review: inReview,
      done,
      closed,
    }),
    [todo, inProgress, inReview, done, closed],
  );
}

/** Rows this surface may render for one column — the window bound applied to
 *  the loaded prefix. */
export function boardVisibleItems(
  column: BoardColumnRead,
  window: ColumnWindow,
  status: TaskStatus,
): TaskSummaryDto[] {
  const bound = columnVisibleCount(window, status);
  return column.items.length > bound ? column.items.slice(0, bound) : column.items;
}

/** Whether growing this column's window would reveal anything more — either
 *  loaded-but-unrendered rows, or a page the server still has. */
export function boardColumnCanGrow(
  column: BoardColumnRead,
  window: ColumnWindow,
  status: TaskStatus,
): boolean {
  return column.items.length > columnVisibleCount(window, status) || !column.atEnd;
}

/**
 * The authoritative per-status totals under the current filters — the ONE
 * source for a column header's number and for the titlebar badge.
 *
 * Returns the AUTHORITY, not a `BoardCounts`: the face always carries all five
 * statuses, so a zeroed fallback would be indistinguishable from a real answer
 * and a column whose body holds rows could print an authoritative `0` in its
 * header. Unknown-ness comes from the query state alone — never from the value
 * (see `boardAuthority.ts`). No count is ever maintained by `±1` on an event.
 */
export function useBoardCounts(
  filters: TaskFilters,
  opts?: { enabled?: boolean },
): BoardAuthority<BoardCounts> {
  const enabled = opts?.enabled ?? true;
  const query = useQuery({
    queryKey: boardCountsKey(boardFilterKey(filters)),
    enabled,
    placeholderData: keepPreviousData,
    queryFn: ({ signal }) =>
      api<TaskBoardCountsDto>(boardCountsPath(boardFilterParams(filters)), {
        signal,
      }),
    select: (dto) => dto.counts as BoardCounts,
  });
  const overdue = usePlaceholderOverdue(query.isPlaceholderData);
  const data = overdue ? undefined : query.data;
  const { isError } = query;
  return useMemo(() => boardAuthority({ data, isError }), [data, isError]);
}

/**
 * The filter menus' option sets, over the population of ONE container (or all
 * containers when none is selected).
 *
 * Deliberately unfiltered by the within-board axes and unpaginated at the
 * source: option sets must not shrink to the current selection (else picking one
 * person collapses the menu to that person), and they must not shrink to
 * "whoever appeared in the pages we happened to load". The ONE axis they DO
 * honor is `filters.container` — the menus describe the container tab you are on
 * (`workspace` / `agent_space` / `direct_messages`), so switching tabs reshapes
 * them, and each container keeps its own cache entry.
 *
 * Returns the AUTHORITY for the same reason as the counts face, with the sharper
 * edge that here the empty answer is meaningful: a viewer whose visible
 * population really is empty is answered `[]`, so a failed cold read must not be
 * able to present itself as that (#task:1885).
 */
export function useBoardFacets(
  filters: TaskFilters,
  opts?: {
    enabled?: boolean;
  },
): BoardAuthority<BoardFacets> {
  const enabled = opts?.enabled ?? true;
  const container = filters.container;
  const query = useQuery({
    queryKey: boardFacetsKey(container),
    enabled,
    // A container switch keeps the old avatars until the new set lands, so the
    // toolbar row does not collapse and reflow.
    placeholderData: keepPreviousData,
    queryFn: ({ signal }) => api<TaskBoardFacetsDto>(boardFacetsPath(container), { signal }),
    select: (dto): BoardFacets => {
      const toOptions = (people: TaskBoardFacetsDto["assignees"]): PersonOption[] =>
        people.map((p) => ({ person: { type: p.type, id: p.id }, name: p.name }));
      return {
        assignees: toOptions(dto.assignees),
        creators: toOptions(dto.creators),
        hasUnassigned: dto.has_unassigned,
      };
    },
  });
  const { data, isError } = query;
  return useMemo(() => boardAuthority({ data, isError }), [data, isError]);
}

/**
 * Keep a mounted board fresh (viewer scope: the server fans `task_changed` out
 * to each human member of the task's conversation, so one subscription covers
 * every conversation the viewer belongs to, across workspaces).
 *
 * ONE OWNER: called by App, never by a board surface. App is the ancestor of both
 * surfaces AND of the titlebar badge, so a single listener there keeps every
 * board projection fresh whether a board is mounted or not. Two listeners would
 * not be a free dedup — a second `invalidateQueries` on a key that already has
 * data CANCELS the in-flight refetch and restarts it (`cancelRefetch` defaults to
 * true), so each event would cost an extra round trip per surface.
 *
 * PATCH WHAT THE EVENT PROVES, RE-READ ONLY THE REST (#task:1975). The event is
 * an absolute snapshot on an ordered per-scope stream with replay and
 * `snapshot_required` fencing, so it may be ASSIGNED — and the board's own cache
 * supplies the two things the payload lacks: the column the row is LEAVING (it is
 * the column we hold it in — `previous_status` was never needed for that) and the
 * full `TaskSummaryDto` being moved. `applyTaskChangedToProjections` does that for
 * every column, chip and open detail this client holds.
 *
 * What is still re-read, and why only then:
 *   - column bodies of the scopes involved: the move is exact for the loaded
 *     prefix, but a row that was sitting just past the window belongs in the
 *     source column's first page now. Scoped by `reconcileTaskStatusWrite` to the
 *     source/destination pair per filter scope — plus every column of a scope
 *     whose window could not place the row at all, which is the honest answer to
 *     "we do not know where it was".
 *   - counts: server `GROUP BY`, never a client `±1`.
 *   - facets: only `create` / `claim` / `unclaim`, the kinds that can change who
 *     appears as an assignee. A `status` write cannot, and used to refetch them
 *     anyway through the `['tasks']` root.
 *
 * AND NOTHING AT ALL when the event brought no news: `needsReconcile` is false
 * once the caches already agree on status and assignee, which is exactly the
 * state the writer's own optimistic patch leaves them in. That is how the local
 * write and its echo stop being two refetch owners — by VALUE, with no mutation
 * identity, no `previous_status` and no self-echo timing window. The same
 * comparison absorbs a replayed frame and a second delivery of one commit (the
 * event is published on two scopes with independent seq spaces, so duplicate
 * arrival is normal, not exceptional — Alice's wire audit on #task:1975).
 */
export function useTaskBoardRealtime(): void {
  const qc = useQueryClient();
  useEffect(() => {
    const sub = subscribeViewerScope();
    const unsub = onWsFrame((frame) => {
      if (frame.type === "snapshot_required") {
        if (isCurrentViewerScope(frame.scope)) {
          void qc.invalidateQueries({ queryKey: ["task-by-number"] });
          void qc.invalidateQueries({ queryKey: ["tasks"] });
        }
        return;
      }
      if (frame.type !== "event") return;
      if (frame.kind !== "task_changed" || !isCurrentViewerScope(frame.scope)) {
        return;
      }
      const payload = frame.payload as TaskChangedEvent;
      const outcome = applyTaskChangedToProjections(qc, payload);
      if (payload.kind === "create" || payload.kind === "claim" || payload.kind === "unclaim") {
        // Facets are keyed per container, so refresh EVERY container's menus:
        // the changed task belongs to exactly one, but this listener does not
        // resolve which, and the prefix match covers them all (including the
        // All entry) in one call.
        void qc.invalidateQueries({ queryKey: ["tasks", "board", "facets"] });
      }
      if (!outcome.needsReconcile) return;
      reconcileTaskStatusWrite(qc, outcome.projection, {
        // The detail projection's refetch has exactly one owner (`useTaskDetail`
        // via `shouldRefreshTaskDetail`), and an `edit` is the only kind whose
        // authority is wider than its own snapshot.
        detail: false,
      });
    });
    return () => {
      sub.unsubscribe();
      unsub();
    };
  }, [qc]);

  // §4-A channel-name fact edge (#task:2041). A board card's `channel_name` is a
  // LIVE projection of the channel title, not a value frozen onto the task at
  // create time — a channel rename/archive must move it. That edit fires
  // `workspace_channels_changed` on the channel's `workspace:<id>` scope with NO
  // `task_changed` (nothing about the task changed), so the viewer-scope listener
  // above never sees it. The board is cross-workspace, so subscribe on EVERY
  // workspace the viewer belongs to and, on that event, re-read only the columns
  // face: the cards carry the name, while counts and facets do not, and no row
  // moves — the rename changes a label, not a placement.
  const { data: workspaces = [] } = useWorkspaces();
  // A stable, order-independent key over the id set, so the effect re-subscribes
  // only when the membership actually changes (not on every list refetch that
  // returns equal ids in a fresh array).
  const workspaceScopeKey = useMemo(
    () => [...workspaces.map((w) => w.id)].sort().join(","),
    [workspaces],
  );
  useEffect(() => {
    const ids = workspaceScopeKey ? workspaceScopeKey.split(",") : [];
    if (ids.length === 0) return;
    const scopes = new Set(ids.map((id) => `workspace:${id}`));
    const subs = ids.map((id) => subscribeWsScope(`workspace:${id}`));
    const unsub = onWsFrame((frame) => {
      // Both the live event and a post-reconnect `snapshot_required` on one of
      // these scopes can carry a missed rename, so both re-read.
      const hit =
        (frame.type === "event" &&
          frame.kind === "workspace_channels_changed" &&
          scopes.has(frame.scope)) ||
        (frame.type === "snapshot_required" && scopes.has(frame.scope));
      if (!hit) return;
      void qc.invalidateQueries({ queryKey: ["tasks", "board", "column"] });
    });
    return () => {
      for (const s of subs) s.unsubscribe();
      unsub();
    };
  }, [qc, workspaceScopeKey]);
}
