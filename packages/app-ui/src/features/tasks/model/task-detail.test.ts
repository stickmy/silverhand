import { describe, expect, it, vi } from "vitest";
import { QueryClient } from "@tanstack/react-query";
import type { TaskDetailDto, TaskSummaryDto } from "@silverhand/shared";
import { ApiError } from "@/lib/api";
import {
  classifyTaskDetailFailure,
  resolveTaskDetailView,
  resolveTaskJumpTarget,
  shouldRefreshTaskDetail,
  shouldRefreshTaskHistory,
  shouldRetryTaskDetail,
  taskDetailKey,
  type TaskDetailFrame,
} from "./task-detail";

function summary(overrides: Partial<TaskSummaryDto> = {}): TaskSummaryDto {
  return {
    id: "task-1",
    task_number: 7,
    place: {
      kind: "workspace",
      conversation_id: "anchor-chan",
      workspace_id: "ws1",
      channel_name: "chan",
    },
    thread_root_id: null,
    status: "todo",
    title: "a task",
    assignee: null,
    creator: { type: "user", id: "u1", name: "Shiki" },
    created_at: 1000,
    closed_at: null,
    ...overrides,
  };
}

function detail(overrides: Partial<TaskDetailDto> = {}): TaskDetailDto {
  return {
    ...summary(),
    content: "body",
    last_editor: null,
    last_edited_at: null,
    source_message_id: "src-msg",
    nav: { kind: "unavailable" },
    ...overrides,
  };
}

describe("resolveTaskJumpTarget", () => {
  it('navigates to the source ref when nav.kind is "source"', () => {
    const d = detail({
      nav: {
        kind: "source",
        ref: {
          conversation_id: "src-chan",
          thread_root_id: "src-thread",
          message_id: "src-msg",
        },
      },
    });
    expect(resolveTaskJumpTarget(d)).toEqual({
      workspaceId: "ws1",
      channelId: "src-chan",
      threadRootId: "src-thread",
      messageId: "src-msg",
    });
  });

  it('navigates to the created-event ref when nav.kind is "created_event"', () => {
    const d = detail({
      source_message_id: null,
      nav: {
        kind: "created_event",
        ref: {
          conversation_id: "evt-chan",
          thread_root_id: null,
          message_id: "evt-msg",
        },
      },
    });
    expect(resolveTaskJumpTarget(d)).toEqual({
      workspaceId: "ws1",
      channelId: "evt-chan",
      threadRootId: null,
      messageId: "evt-msg",
    });
  });

  it("navigates a direct_pair task to its pair conversation, without a workspaceId", () => {
    // A pair-placed task has no workspace coordinate, but its ref
    // conversation_id IS
    // the pair conversation id — the caller routes that through the viewer's
    // pair carrier (`directPairIds` → `nav.toDirect`). Suppressing the jump for
    // a non-workspace placement would be a shape side-effect, not a real
    // fail-closed: the ref is resolvable.
    const d = detail({
      place: { kind: "direct_pair", conversation_id: "pair-conv", pair_key: "u1:u2" },
      nav: {
        kind: "source",
        ref: {
          conversation_id: "pair-conv",
          thread_root_id: null,
          message_id: "pair-msg",
        },
      },
    });
    expect(resolveTaskJumpTarget(d)).toEqual({
      workspaceId: undefined,
      channelId: "pair-conv",
      threadRootId: null,
      messageId: "pair-msg",
    });
  });

  it('returns null when nav.kind is "unavailable" (caller hides the affordance)', () => {
    const d = detail({ nav: { kind: "unavailable" } });
    expect(resolveTaskJumpTarget(d)).toBeNull();
  });
});

// ── Failure contract ───────────────────────────────────────────────
//
// This is an ERROR contract, not a UI assertion: which tier a failure lands in,
// whether it may retry, and what the modal is allowed to paint for it.

describe("classifyTaskDetailFailure", () => {
  it("reads an authoritative 404 as not_found", () => {
    expect(classifyTaskDetailFailure(new ApiError(404, "Not Found", ""))).toBe("not_found");
  });

  it("reads a backend_unreachable ApiError as unreachable", () => {
    expect(
      classifyTaskDetailFailure(new ApiError(0, "Network Error", "", "backend_unreachable")),
    ).toBe("unreachable");
  });

  it("reads every other failure as transient", () => {
    expect(classifyTaskDetailFailure(new ApiError(500, "Server Error", ""))).toBe("transient");
    expect(classifyTaskDetailFailure(new Error("malformed json"))).toBe("transient");
  });
});

describe("shouldRetryTaskDetail", () => {
  it("never retries a 404 — it is terminal, a second read cannot change it", () => {
    expect(shouldRetryTaskDetail(0, new ApiError(404, "Not Found", ""))).toBe(false);
  });

  it("keeps the client-wide single retry for the other tiers", () => {
    const server = new ApiError(500, "Server Error", "");
    expect(shouldRetryTaskDetail(0, server)).toBe(true);
    expect(shouldRetryTaskDetail(1, server)).toBe(false);
    const down = new ApiError(0, "Network Error", "", "backend_unreachable");
    expect(shouldRetryTaskDetail(0, down)).toBe(true);
    expect(shouldRetryTaskDetail(1, down)).toBe(false);
  });

  it("spends exactly ONE request on a 404 as the query is wired", async () => {
    // Request-count falsifier, not just the predicate: the policy only pays off
    // if it is the query's `retry`. A plain `QueryClient` (no React) is enough to
    // count, so this stays a cache/request contract rather than a UI test.
    const client = new QueryClient({
      defaultOptions: { queries: { retry: 1, retryDelay: 0 } },
    });
    const gone = vi.fn(async () => {
      throw new ApiError(404, "Not Found", "");
    });
    await expect(
      client.fetchQuery({
        queryKey: taskDetailKey("gone"),
        queryFn: gone,
        retry: shouldRetryTaskDetail,
      }),
    ).rejects.toBeInstanceOf(ApiError);
    expect(gone).toHaveBeenCalledTimes(1);

    // Control: the same wiring still auto-retries a 5xx once, so the 1 above is
    // the policy at work and not a client that stopped retrying altogether.
    const flaky = vi.fn(async () => {
      throw new ApiError(500, "Server Error", "");
    });
    await expect(
      client.fetchQuery({
        queryKey: taskDetailKey("flaky"),
        queryFn: flaky,
        retry: shouldRetryTaskDetail,
      }),
    ).rejects.toBeInstanceOf(ApiError);
    expect(flaky).toHaveBeenCalledTimes(2);
  });
});

describe("resolveTaskDetailView", () => {
  const d = detail();

  it("paints the body on success, and loads when there is nothing yet", () => {
    expect(resolveTaskDetailView({ failure: null, detail: d })).toEqual({
      kind: "body",
      detail: d,
    });
    expect(resolveTaskDetailView({ failure: null, detail: undefined })).toEqual({
      kind: "loading",
    });
  });

  // Each tier is resolved TWICE — cold-load and "had data, then failed" — because
  // TanStack retains `data` through a failed background refetch, so the second
  // column is a normal state for an open modal, not an edge case.
  it("lets the failure win outright on not_found, cached body or not", () => {
    expect(resolveTaskDetailView({ failure: "not_found", detail: undefined })).toEqual({
      kind: "not_found",
    });
    expect(resolveTaskDetailView({ failure: "not_found", detail: d })).toEqual({
      kind: "not_found",
    });
  });

  it("keeps a Retry reachable on transient, cached body or not", () => {
    expect(resolveTaskDetailView({ failure: "transient", detail: undefined })).toEqual({
      kind: "transient_empty",
    });
    expect(resolveTaskDetailView({ failure: "transient", detail: d })).toEqual({
      kind: "body_with_retry",
      detail: d,
    });
  });

  it("keeps reading the cached body on unreachable, and never fakes loading without one", () => {
    expect(resolveTaskDetailView({ failure: "unreachable", detail: d })).toEqual({
      kind: "body_stale",
      detail: d,
    });
    const empty = resolveTaskDetailView({
      failure: "unreachable",
      detail: undefined,
    });
    expect(empty).toEqual({ kind: "unreachable_empty" });
    // A spinner that will never resolve is a lie — `loading` must be
    // unreachable for any failure tier.
    expect(empty.kind).not.toBe("loading");
  });

  it("never lets data decide the TIER", () => {
    // Same data, three failures → three different kinds; same failure, two data
    // states → same tier. That is the whole point of the two axes.
    const kinds = (["not_found", "transient", "unreachable"] as const).map(
      (f) => resolveTaskDetailView({ failure: f, detail: d }).kind,
    );
    expect(new Set(kinds).size).toBe(3);
  });
});

// ── Realtime ownership ─────────────────────────────────────────────
//
// `['task-detail', id]` has ONE owner. Ownership is decided by the frame's
// SCOPE, not by which hook happens to be mounted: a test whose frame both
// carriers would see cannot attribute the refresh (Alice), so the viewer arms
// and the conversation arm are separated here by `viewerScope`.

describe("shouldRefreshTaskDetail", () => {
  const openTaskId = "task-open";
  const event = (over: Partial<Extract<TaskDetailFrame, { type: "event" }>> = {}) =>
    ({
      type: "event",
      viewerScope: true,
      kind: "task_changed",
      taskId: openTaskId,
      taskKind: "edit",
      ...over,
    }) as TaskDetailFrame;

  it("refreshes on a matching viewer-scope edit (body is not on the wire)", () => {
    expect(shouldRefreshTaskDetail(event(), openTaskId)).toBe(true);
  });

  it("refreshes on a viewer-scope snapshot_required (gap recovery)", () => {
    expect(
      shouldRefreshTaskDetail({ type: "snapshot_required", viewerScope: true }, openTaskId),
    ).toBe(true);
  });

  it("does NOT refresh for kinds the event snapshot already settles", () => {
    // status / claim / unclaim / create carry the full mutable state this
    // projection shares with a board row, and the App-level ingestor assigns it
    // (`applyTaskChangedToProjections`). Re-reading would spend a round trip to
    // learn what the frame just said (#task:1975).
    for (const taskKind of ["status", "claim", "unclaim", "create"] as const) {
      expect(shouldRefreshTaskDetail(event({ taskKind }), openTaskId)).toBe(false);
    }
    // A payload with no kind at all is not evidence of an edit.
    expect(shouldRefreshTaskDetail(event({ taskKind: null }), openTaskId)).toBe(false);
  });

  it("ignores another task, another kind, and unrelated frame types", () => {
    expect(shouldRefreshTaskDetail(event({ taskId: "other" }), openTaskId)).toBe(false);
    expect(shouldRefreshTaskDetail(event({ taskId: null }), openTaskId)).toBe(false);
    expect(shouldRefreshTaskDetail(event({ kind: "message_created" }), openTaskId)).toBe(false);
    expect(shouldRefreshTaskDetail({ type: "other" }, openTaskId)).toBe(false);
  });

  it("ignores conversation-scope frames entirely — including its own task", () => {
    // The conversation ingestor's carrier. Even a `task_changed` for THIS task
    // must not refresh here: two writers on one key means the second
    // `invalidateQueries` cancels and restarts the first's in-flight refetch
    // (`cancelRefetch` defaults to true).
    expect(shouldRefreshTaskDetail(event({ viewerScope: false }), openTaskId)).toBe(false);
    expect(
      shouldRefreshTaskDetail({ type: "snapshot_required", viewerScope: false }, openTaskId),
    ).toBe(false);
  });
});

describe("shouldRefreshTaskHistory", () => {
  const event = (
    over: Partial<Extract<TaskDetailFrame, { type: "event" }>> = {},
  ): TaskDetailFrame => ({
    type: "event",
    viewerScope: true,
    kind: "task_changed",
    taskId: "t-1",
    taskKind: "assign",
    ...over,
  });
  it("re-reads on any change to the open task", () => {
    for (const taskKind of ["create", "claim", "assign", "unclaim", "status", "edit"] as const) {
      expect(shouldRefreshTaskHistory(event({ taskKind }), "t-1")).toBe(true);
    }
  });
  it("ignores other tasks, other scopes and other frames", () => {
    expect(shouldRefreshTaskHistory(event({ taskId: "t-2" }), "t-1")).toBe(false);
    expect(shouldRefreshTaskHistory(event({ viewerScope: false }), "t-1")).toBe(false);
    expect(shouldRefreshTaskHistory(event({ kind: "message_created" }), "t-1")).toBe(false);
    expect(shouldRefreshTaskHistory({ type: "other" }, "t-1")).toBe(false);
  });
  it("re-reads after a viewer snapshot gap", () => {
    expect(shouldRefreshTaskHistory({ type: "snapshot_required", viewerScope: true }, "t-1")).toBe(
      true,
    );
  });
});
