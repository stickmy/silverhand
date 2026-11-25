import type { TaskChangedEvent, TaskDetailDto } from "@silverhand/shared";
import { ApiError } from "@/lib/api";

/** React Query key for a single task's on-demand detail read, keyed by the
 *  task identity (`tasks.id`). Lives here (a pure module) so both the hooks
 *  and the realtime ingestor share one key shape without importing the React
 *  hooks module. */
export const taskDetailKey = (taskId: string | null | undefined) =>
  ["task-detail", taskId ?? null] as const;

/** Where "View in conversation" navigates. Same shape both task-board card
 *  surfaces already pass to their jump handler. */
export interface TaskJumpTarget {
  workspaceId?: string;
  channelId: string;
  threadRootId?: string | null;
  messageId: string;
}

/**
 * Resolve where the detail modal's "View in conversation" navigates, from the
 * server-resolved BRANCH `nav` discriminant. The renderer branches ONLY on
 * `nav.kind` and never guesses a fallback:
 *   - `source` / `created_event` → jump to the carried viewer-resolved `ref`.
 *   - `unavailable`              → `null`: nothing to navigate to (no source and
 *                                   no resolvable event, OR a declared source the
 *                                   viewer can't read). The caller HIDES the
 *                                   affordance — a declared source never silently
 *                                   falls back to the event.
 *
 * `nav` is per-viewer and only present on the resolved detail, so the caller
 * passes `detail` once the read succeeds; `null` here means "render no button".
 *
 * `workspaceId` rides ONLY on the workspace arm — a pair-placed task carries no
 * workspace coordinate. That is not a reason to suppress the jump: the caller
 * resolves a workspace-less `channelId` through the viewer's pair carrier
 * (`directPairIds` → `nav.toDirect`), and `nav.ref.conversation_id` already holds
 * the pair conversation id for a DM task. So both placements navigate whenever
 * the server resolved a ref; only `unavailable` returns `null`.
 *
 * `TaskJumpTarget.channelId` keeps its name on purpose — it is this handler
 * boundary's own vocabulary, shared with every other jump caller. The wire field
 * is `conversation_id` because a pair task's coordinate is not a channel; that
 * asymmetry is a renderer-side naming debt, never an argument for renaming the
 * wire back.
 */
export function resolveTaskJumpTarget(detail: TaskDetailDto): TaskJumpTarget | null {
  if (detail.nav.kind === "unavailable") return null;
  return {
    workspaceId: detail.place.kind === "workspace" ? detail.place.workspace_id : undefined,
    channelId: detail.nav.ref.conversation_id,
    threadRootId: detail.nav.ref.thread_root_id,
    messageId: detail.nav.ref.message_id,
  };
}

/**
 * Why a task-detail read failed, as the modal presents it. Three tiers, because
 * the recovery differs and the URL pointer (`?task=<task_id>`) must survive two
 * of them (#task:1715):
 *
 *   - `not_found` — authoritative 404 (deleted, or the viewer can't read that
 *     conversation; the route answers 404 for both). TERMINAL: no retry can fix
 *     it. The modal still stays open with an explanation and an explicit Close —
 *     silently stripping a pointer the user just clicked leaves them with no
 *     account of what happened (the conversation-pane placeholder settled this).
 *   - `unreachable` — the request never reached the backend
 *     (`ApiError.kind === 'backend_unreachable'`). The global `AppStatusBanner`
 *     already owns that signal AND its Retry, so the modal only explains itself
 *     and adds no second, narrower retry button.
 *   - `transient` — any other HTTP failure (5xx, malformed reply). The query
 *     client is `retry: 1` + `refetchOnWindowFocus: false`, so nothing re-reads
 *     this on its own: the modal MUST carry its own Retry or the pointer is
 *     stuck until the user navigates (Cindy review).
 */
export type TaskDetailFailure = "not_found" | "unreachable" | "transient";

export function classifyTaskDetailFailure(error: unknown): TaskDetailFailure {
  if (error instanceof ApiError) {
    if (error.kind === "backend_unreachable") return "unreachable";
    if (error.status === 404) return "not_found";
  }
  return "transient";
}

/**
 * The query's own `retry` policy, overriding the client-wide `retry: 1`.
 *
 * A 404 here is AUTHORITATIVE and terminal — the task is gone, or this viewer
 * can't read its conversation. Inheriting the global single retry spends a
 * second round trip that cannot change the answer and delays the terminal state
 * by a full request, which is exactly what a stale pointer (an old deep-link, a
 * chip for a deleted task) hits (Cindy review). Every other tier keeps the
 * client-wide behaviour: one automatic retry, then the panel's own Retry.
 */
export function shouldRetryTaskDetail(failureCount: number, error: unknown): boolean {
  if (classifyTaskDetailFailure(error) === "not_found") return false;
  return failureCount < 1;
}

/**
 * What the detail modal renders, as ONE exhaustive discriminant.
 *
 * Failure classification and "is there a body to paint" are two INDEPENDENT
 * axes, and both are real: TanStack keeps the previous `data` when a background
 * refetch fails (`query.js` spreads the old state into the `error` action), so
 * `isError && data !== undefined` is a normal state for an open modal whose
 * exact-key refresh just failed. Branching on the body first — `detail ? body :
 * panel` — silently swallows the failure axis: a 5xx refresh kept painting a
 * body with no reachable Retry, and a 404 kept painting authority the server has
 * just disowned (Cindy BLOCKER 4).
 *
 * So the two axes are resolved TOGETHER, here, and the renderer becomes a
 * `switch` over the result. `data` never decides the TIER (that is the failure's
 * job) — it only decides the SHAPE within a tier:
 *
 *   - `not_found`         — failure wins outright: the stale body is not painted
 *                           even when we still hold one. Terminal; the corner ✕
 *                           is the only exit and the URL pointer survives.
 *   - `transient_*`       — a Retry is reachable whether or not a body is shown.
 *   - `unreachable_*`     — with a body: keep reading it, and let the ONE
 *                           app-status banner own the connection signal (a
 *                           second copy of it here would be a second surface
 *                           claiming the same fact, and a second Retry owner).
 *                           Without one: still a non-success state — a spinner
 *                           that will never resolve is a lie.
 *   - `loading`           — a distinct kind, so "permanent loading" is not
 *                           expressible for any failure tier.
 */
export type TaskDetailView =
  | { kind: "loading" }
  | { kind: "body"; detail: TaskDetailDto }
  /** transient failure over a cached body — body + inline Retry. */
  | { kind: "body_with_retry"; detail: TaskDetailDto }
  /** unreachable over a cached body — body as-is; the banner owns the signal. */
  | { kind: "body_stale"; detail: TaskDetailDto }
  | { kind: "not_found" }
  | { kind: "transient_empty" }
  | { kind: "unreachable_empty" };

export function resolveTaskDetailView(args: {
  failure: TaskDetailFailure | null;
  detail: TaskDetailDto | undefined;
}): TaskDetailView {
  const { failure, detail } = args;
  switch (failure) {
    case null:
      return detail ? { kind: "body", detail } : { kind: "loading" };
    case "not_found":
      return { kind: "not_found" };
    case "transient":
      return detail ? { kind: "body_with_retry", detail } : { kind: "transient_empty" };
    case "unreachable":
      return detail ? { kind: "body_stale", detail } : { kind: "unreachable_empty" };
  }
}

/**
 * A realtime frame as the task-detail projection sees it. `useTaskDetail` is the
 * SOLE owner of `['task-detail', id]`'s REFETCH (#task:1715), and ownership is
 * decided by the frame's SCOPE, not by which hook happens to be mounted: this
 * projection only listens on the viewer's private scope (`viewer:<userId>`),
 * while the conversation ingestor works `conversation:<id>` frames. A
 * conversation-scope frame therefore reaches `viewerScope: false` and refreshes
 * nothing — if it did, we would be back to two writers invalidating one key, and
 * a duplicate `invalidateQueries` on a query that already has data CANCELS the
 * in-flight refetch and restarts it (`cancelRefetch` defaults to true).
 *
 * `kind` is the FRAME's kind (`'task_changed'`); `taskKind` is the task
 * mutation's kind inside the payload, which is what decides whether the event's
 * snapshot is wide enough to settle this projection by assignment.
 */
export type TaskDetailFrame =
  | { type: "snapshot_required"; viewerScope: boolean }
  | {
      type: "event";
      viewerScope: boolean;
      kind: string;
      taskId: string | null;
      taskKind: TaskChangedEvent["kind"] | null;
    }
  /** Any other frame type — heartbeats, acks, unrelated envelopes. */
  | { type: "other" };

/**
 * Whether an open detail modal still has to RE-READ after this frame.
 *
 * A `task_changed` event carries the full mutable state the detail shares with
 * the board row (status, assignee, title, `closed_at`), and the App-level
 * ingestor assigns it to every task projection this client holds, including this
 * one (`applyTaskChangedToProjections`). So a `status` / `claim` / `unclaim` /
 * `create` event is already settled by value and re-reading it would spend a
 * round trip to learn what the frame just said (#task:1975).
 *
 * `edit` is the one kind whose authority is WIDER than its event: the body, the
 * "edited by" byline and the edit timestamp are deliberately not on the wire
 * (the event stays thin), so only a detail read can produce them.
 *
 * Assignment and re-reading are split on purpose: assigning the same snapshot
 * twice is free and order-independent, whereas two owners of a REFETCH cost a
 * cancelled round trip each time. This function keeps that second half single.
 */
export function shouldRefreshTaskDetail(frame: TaskDetailFrame, openTaskId: string): boolean {
  switch (frame.type) {
    case "snapshot_required":
      // Gap recovery: we may have missed this task's change while
      // disconnected, so re-read rather than trust what is cached — the one path
      // where nothing local can be trusted to be a projection of the authority.
      return frame.viewerScope;
    case "event":
      if (!frame.viewerScope) return false;
      if (frame.kind !== "task_changed") return false;
      if (frame.taskId !== openTaskId) return false;
      return frame.taskKind === "edit";
    case "other":
      return false;
  }
}

/** Whether the open task's history has to re-read: every committed change to it
 *  appends an event the `task_changed` frame does not carry. */
export function shouldRefreshTaskHistory(frame: TaskDetailFrame, openTaskId: string): boolean {
  switch (frame.type) {
    case "snapshot_required":
      return frame.viewerScope;
    case "event":
      return frame.viewerScope && frame.kind === "task_changed" && frame.taskId === openTaskId;
    case "other":
      return false;
  }
}
