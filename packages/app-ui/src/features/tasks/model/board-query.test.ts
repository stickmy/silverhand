import { describe, expect, it } from "vitest";
import { TaskBoardColumnQuerySchema, TaskBoardCountsQuerySchema } from "@silverhand/shared";
import {
  boardColumnKey,
  boardColumnPath,
  boardCountsKey,
  boardCountsPath,
  boardFacetsKey,
  boardFilterKey,
  boardFilterParams,
  classifyTasksCacheKey,
} from "./board-query";
import { COLUMN_PAGE_SIZE } from "./column-window";
import { DEFAULT_TASK_FILTERS, type TaskFilters } from "./task-filters";

function filters(overrides: Partial<TaskFilters>): TaskFilters {
  return { ...DEFAULT_TASK_FILTERS, created: "all", ...overrides };
}

/** Query params as the server will actually receive them. */
function parseQuery(path: string): Record<string, string> {
  const qs = path.slice(path.indexOf("?") + 1);
  return Object.fromEntries(new URLSearchParams(qs).entries());
}

const ALL_AXES = filters({
  workspaceId: "ws1",
  channelId: "ch1",
  assignee: { kind: "person", person: { type: "agent", id: "a1" } },
  creator: { kind: "person", person: { type: "user", id: "u1" } },
});

describe("boardFilterParams", () => {
  it("maps every axis the board narrows on", () => {
    expect(boardFilterParams(ALL_AXES)).toEqual({
      workspace_id: "ws1",
      channel_id: "ch1",
      assignee_type: "agent",
      assignee_id: "a1",
      creator_type: "user",
      creator_id: "u1",
    });
  });

  it("omits every axis when nothing narrows", () => {
    expect(boardFilterParams(filters({ created: "all" }))).toEqual({});
  });

  it("sends the creation window as a canonical epoch-ms lower bound", () => {
    const p = boardFilterParams(filters({ created: "week" }));
    expect(p.created_after).toMatch(/^[1-9][0-9]*$/);
    expect(boardFilterKey(filters({ created: "week" }))).not.toEqual(
      boardFilterKey(filters({ created: "all" })),
    );
  });

  it("sends the unassigned assignee axis with no id", () => {
    const p = boardFilterParams(filters({ assignee: { kind: "unassigned" } }));
    expect(p).toEqual({ assignee_type: "unassigned" });
  });

  // The creator axis has no `unassigned` value on the wire and the menu never
  // offers one; the shared PersonFilter type can still spell it. Narrowing
  // nothing beats sending a param the schema rejects with a 400.
  it("narrows nothing for an unassigned creator", () => {
    expect(boardFilterParams(filters({ creator: { kind: "unassigned" } }))).toEqual({});
  });
});

describe("request shape against the authority schemas", () => {
  const CASES: Array<[string, TaskFilters]> = [
    ["defaults", DEFAULT_TASK_FILTERS],
    ["all axes", ALL_AXES],
    ["workspace only", filters({ workspaceId: "ws1" })],
    ["channel only", filters({ channelId: "ch1" })],
    ["unassigned assignee", filters({ assignee: { kind: "unassigned" } })],
    [
      "user assignee",
      filters({ assignee: { kind: "person", person: { type: "user", id: "u1" } } }),
    ],
    ["unassigned creator (unreachable)", filters({ creator: { kind: "unassigned" } })],
  ];

  // Every request this mapper can build must satisfy the shared query schema —
  // in particular the half-specified person axis it refuses (400). This is the
  // seam test: the renderer's params are checked against the authority's own
  // definition, not against a copy of it.
  it.each(CASES)("column request is valid: %s", (_name, f) => {
    const params = parseQuery(boardColumnPath("todo", boardFilterParams(f)));
    expect(TaskBoardColumnQuerySchema.safeParse(params).success).toBe(true);
  });

  it.each(CASES)("counts request is valid: %s", (_name, f) => {
    const params = parseQuery(boardCountsPath(boardFilterParams(f)));
    expect(TaskBoardCountsQuerySchema.safeParse(params).success).toBe(true);
  });
});

describe("boardColumnPath", () => {
  it("requests one page per read, so one window page consumes one fetch", () => {
    const params = parseQuery(boardColumnPath("in_review", {}));
    expect(params.limit).toBe(String(COLUMN_PAGE_SIZE));
    expect(params.status).toBe("in_review");
  });

  it("omits the cursor for the first window", () => {
    expect(parseQuery(boardColumnPath("todo", {})).cursor).toBeUndefined();
    expect(parseQuery(boardColumnPath("todo", {}, null)).cursor).toBeUndefined();
  });

  it("carries a continuation cursor verbatim (opaque encoding)", () => {
    expect(parseQuery(boardColumnPath("todo", {}, "c:42:x")).cursor).toBe("c:42:x");
  });
});

describe("classifyTasksCacheKey", () => {
  // Every key this module builds must be classifiable: a walker over the shared
  // `['tasks', …]` root dispatches on this, so an unclassified face silently
  // becomes "skip" (rows of a lost channel linger) or a shape crash.
  it("classifies every key builder in this module", () => {
    const filterKey = boardFilterKey(DEFAULT_TASK_FILTERS);
    expect(classifyTasksCacheKey(boardColumnKey("todo", filterKey))).toBe("board-column");
    expect(classifyTasksCacheKey(boardCountsKey(filterKey))).toBe("board-counts");
    expect(classifyTasksCacheKey(boardFacetsKey())).toBe("board-facets");
  });

  it("refuses to guess: a new board face, a non-board shape, and a foreign root", () => {
    expect(classifyTasksCacheKey(["tasks", "board", "ranked"])).toBe("unknown");
    // The fetch-all list's key shape (#task:1892 retired it). It must classify as
    // `unknown` and NOT as a row array: the walkers skip an unknown, whereas the
    // old `'list'` arm would hand whatever a future non-board face stores under
    // this root to a `.filter` over rows.
    expect(classifyTasksCacheKey(["tasks", "*", "*", "all"])).toBe("unknown");
    expect(classifyTasksCacheKey(["tasks", "ws1", "conv1", "todo"])).toBe("unknown");
    expect(classifyTasksCacheKey(["task-detail", "t1"])).toBe("unknown");
  });
});

describe("boardFilterKey", () => {
  it("is stable across equal filter values", () => {
    expect(boardFilterKey(ALL_AXES)).toBe(
      boardFilterKey({
        ...ALL_AXES,
        assignee: { kind: "person", person: { type: "agent", id: "a1" } },
      }),
    );
  });

  it("separates each axis", () => {
    const keys = new Set(
      [
        DEFAULT_TASK_FILTERS,
        filters({ workspaceId: "ws1" }),
        filters({ channelId: "ch1" }),
        filters({ assignee: { kind: "unassigned" } }),
        filters({ assignee: { kind: "person", person: { type: "agent", id: "a1" } } }),
        filters({ assignee: { kind: "person", person: { type: "user", id: "a1" } } }),
        filters({ creator: { kind: "person", person: { type: "user", id: "u1" } } }),
      ].map(boardFilterKey),
    );
    expect(keys.size).toBe(7);
  });

  // Two filter states that request the SAME window must share one cache entry,
  // or both surfaces fetch the identical page twice under two keys.
  it("collapses states that narrow identically", () => {
    expect(boardFilterKey(filters({ creator: { kind: "unassigned" } }))).toBe(
      boardFilterKey(filters({})),
    );
  });

  it("keys each column separately under the shared tasks root", () => {
    const k = boardColumnKey("done", boardFilterKey(DEFAULT_TASK_FILTERS));
    expect(k[0]).toBe("tasks");
    expect(k).not.toEqual(boardColumnKey("todo", boardFilterKey(DEFAULT_TASK_FILTERS)));
  });
});
