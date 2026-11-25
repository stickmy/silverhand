import {
  apiPaths,
  type TaskBoardAssigneeFilterType,
  type TaskBoardContainer,
  type TaskBoardCreatorFilterType,
  type TaskStatus,
} from "@silverhand/shared";
import { COLUMN_PAGE_SIZE } from "./column-window";
import { createdWindowStart, type TaskFilters } from "./task-filters";

/**
 * Wire mapping for the board read faces (#task:1884) — the pure half of the
 * board's data layer: `TaskFilters` (the UI's filter model) → the query params
 * and cache keys of `/api/tasks/board/{column,counts,facets}`.
 *
 * Kept separate from the hooks so the mapping is unit-testable without React
 * Query, and so ONE definition serves all three faces: the shared filter axes
 * are built once (`boardFilterParams`) and each path appends only its own
 * params. A per-face param builder would let the pane's column and its counts
 * disagree about what "the current filter" means — the board would then show a
 * total that its rows contradict.
 *
 * Cache keys live UNDER the `['tasks', …]` root on purpose. Every existing task
 * mutation already invalidates that root, so the board is refreshed by
 * construction rather than by a list of call sites kept in sync by hand (and the
 * viewer-local key classification in `features/channels/viewer-channel-removal`
 * keeps covering it — a NEW root there would go red in its smoke test).
 */

export interface BoardFilterParams {
  container?: TaskBoardContainer;
  workspace_id?: string;
  channel_id?: string;
  assignee_type?: TaskBoardAssigneeFilterType;
  assignee_id?: string;
  creator_type?: TaskBoardCreatorFilterType;
  creator_id?: string;
  created_after?: string;
}

/**
 * The filter axes every face shares.
 *
 * The person axes are `{type,id}` pairs on the wire and the server rejects a
 * half-specified one (400), so both halves are written together or neither is.
 *
 * The creator axis has NO `unassigned` value by contract — a task always has a
 * creator — and the creator menu never offers one. `PersonFilter` can still
 * SPELL `{kind:'unassigned'}` because it is one shared type for both axes, so
 * this mapper cannot express such a selection: it narrows nothing rather than
 * sending a param the schema would reject. The unreachable selection therefore
 * degrades to the unnarrowed column instead of a failed read.
 */
export function boardFilterParams(filters: TaskFilters): BoardFilterParams {
  const params: BoardFilterParams = {};
  if (filters.container) params.container = filters.container;
  if (filters.workspaceId) params.workspace_id = filters.workspaceId;
  if (filters.channelId) params.channel_id = filters.channelId;
  if (filters.assignee.kind === "unassigned") {
    params.assignee_type = "unassigned";
  } else if (filters.assignee.kind === "person") {
    params.assignee_type = filters.assignee.person.type;
    params.assignee_id = filters.assignee.person.id;
  }
  if (filters.creator.kind === "person") {
    params.creator_type = filters.creator.person.type;
    params.creator_id = filters.creator.person.id;
  }
  const createdAfter = createdWindowStart(filters.created);
  if (createdAfter !== null) params.created_after = String(createdAfter);
  return params;
}

/**
 * One stable string identifying a filter combination — the cache-key segment
 * shared by every face.
 *
 * A single segment (rather than one array element per axis) is what lets the two
 * surfaces SHARE a page cache: both derive the key from the same filters, so the
 * `/tasks` section and the per-conversation Tasks tab read one query per column instead of fetching the
 * same page twice under two shapes. It is derived from the wire params, not from
 * the UI model, so two filter states that request the identical window (e.g. a
 * creator selection this mapper cannot express) resolve to one cache entry
 * instead of silently duplicating the fetch.
 */
export function boardFilterKey(filters: TaskFilters): string {
  const p = boardFilterParams(filters);
  const assignee =
    p.assignee_type === undefined
      ? "any"
      : p.assignee_id === undefined
        ? p.assignee_type
        : `${p.assignee_type}:${p.assignee_id}`;
  // `creator_type` and `creator_id` are written together above (the wire rejects
  // a half-specified axis), so one branch covers both.
  const creator = p.creator_type === undefined ? "any" : `${p.creator_type}:${p.creator_id}`;
  // Container is APPENDED, not prepended: `filterKeyConstrainsAssignee` reads
  // segment [2], and every other positional reader is anchored the same way, so
  // a new axis must land past the existing ones.
  return [
    p.workspace_id ?? "*",
    p.channel_id ?? "*",
    assignee,
    creator,
    p.container ?? "*",
    p.created_after ?? "*",
  ].join("|");
}

/**
 * Does this filter scope narrow the ASSIGNEE axis?
 *
 * Read from the key rather than the filters because the consumers of this
 * predicate hold cache keys, not `TaskFilters` — and it lives here, next to the
 * only place that authors the format, so the two cannot drift.
 *
 * Why the axis matters on its own: an assignee change moves a row between
 * assignee-filtered POPULATIONS without changing its status, so no column move
 * can be derived from it. A scope that narrows nothing on that axis keeps the
 * row it holds (membership cannot have changed) and needs no re-read; a scope
 * that narrows it must re-read the one column holding the row (#task:1975).
 */
export function filterKeyConstrainsAssignee(filterKey: string): boolean {
  const assignee = filterKey.split("|")[2];
  return assignee !== undefined && assignee !== "any";
}

/** Cache key for ONE status column's page list. Status is its own segment so a
 *  single column can be invalidated without touching its neighbours. */
export const boardColumnKey = (status: TaskStatus, filterKey: string) =>
  ["tasks", "board", "column", status, filterKey] as const;

export const boardCountsKey = (filterKey: string) =>
  ["tasks", "board", "counts", filterKey] as const;

/** Facets take EXACTLY ONE axis — the container that selects the board — and no
 *  within-board filters (the option sets must not shrink to the current
 *  assignee/creator/channel selection). So there is one facets cache entry PER
 *  container, and the `create/claim/unclaim` invalidation walks the whole
 *  `['tasks','board','facets']` prefix to refresh every one. */
export const boardFacetsKey = (container?: TaskBoardContainer | null) =>
  ["tasks", "board", "facets", container ?? "*"] as const;

function toSearchParams(params: BoardFilterParams): URLSearchParams {
  const qs = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined) qs.set(k, v);
  }
  return qs;
}

/**
 * `cursor` omitted (or null) = the column's first window.
 *
 * `limit` is always sent explicitly: the window grows by `COLUMN_PAGE_SIZE`
 * rows, so growing it by one page must consume exactly one fetched page. Leaning
 * on the server's default would make that a coincidence between two constants
 * that nothing rechecks.
 */
export function boardColumnPath(
  status: TaskStatus,
  params: BoardFilterParams,
  cursor?: string | null,
): string {
  const qs = toSearchParams(params);
  qs.set("status", status);
  qs.set("limit", String(COLUMN_PAGE_SIZE));
  if (cursor) qs.set("cursor", cursor);
  return `${apiPaths.taskBoardColumn}?${qs.toString()}`;
}

export function boardCountsPath(params: BoardFilterParams): string {
  const qs = toSearchParams(params).toString();
  return qs ? `${apiPaths.taskBoardCounts}?${qs}` : apiPaths.taskBoardCounts;
}

/** The facets face honors ONLY the container axis (see `boardFacetsKey`): the
 *  within-board params are never sent, so the menus describe the whole
 *  container population rather than the current selection. An invalid container
 *  is the server's 400, never a silent fall back to the All menus. */
export function boardFacetsPath(container?: TaskBoardContainer | null): string {
  if (!container) return apiPaths.taskBoardFacets;
  const qs = new URLSearchParams({ container });
  return `${apiPaths.taskBoardFacets}?${qs.toString()}`;
}

/**
 * What kind of value a `['tasks', …]` cache entry holds.
 *
 * Three shapes live under this one root — infinite pages of `{items}`, a counts
 * object, and a facets object — so any cache walker over the root MUST dispatch
 * on the key instead of assuming a shape. Duck-typing them (`data.filter`,
 * `data.pages`) is what turns a new face into a runtime crash in an unrelated
 * code path; the key is the one thing that is always there.
 *
 * `'unknown'` is the FALLTHROUGH, not just a leftover branch: anything that is
 * not one of the three board faces — a `['tasks','board',<new face>]`, or a
 * non-board `['tasks', …]` shape someone writes tomorrow — resolves to it, and a
 * walker must SKIP it rather than guess. That is the fail-closed direction: the
 * retired fetch-all list used to be classified as a row array here (#task:1892),
 * so a new value under this root would have been duck-typed into that arm. The
 * face's author is expected to classify it (see `boardQuery.test.ts`, which
 * asserts every key builder in this module resolves to a known kind).
 */
export type TasksCacheKind = "board-column" | "board-counts" | "board-facets" | "unknown";

export function classifyTasksCacheKey(key: readonly unknown[]): TasksCacheKind {
  if (key[0] !== "tasks") return "unknown";
  if (key[1] !== "board") return "unknown";
  switch (key[2]) {
    case "column":
      return "board-column";
    case "counts":
      return "board-counts";
    case "facets":
      return "board-facets";
    default:
      return "unknown";
  }
}
