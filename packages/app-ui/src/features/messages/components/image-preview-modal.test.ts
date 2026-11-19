import { describe, expect, it } from "vitest";
import { QueryClient } from "@tanstack/react-query";
import type { MessageDto, TaskSummaryDto } from "@silverhand/shared";
import { taskBoardCacheScope, taskByNumberKey } from "@/features/tasks/hooks/use-task-by-number";
import { snapshotStaticTaskRefs } from "./image-preview-modal";

/**
 * The image export freezes each `#task:<n>`'s CURRENT resolved title into the
 * static screenshot. The title must come from the SAME by-number cache cell the
 * live chip resolved — projected from the conversation's placement, not from
 * whatever coordinate happens to be non-null. A wrong scope is a SILENT failure:
 * it reads an empty cell and every task ref exports as a bare number, which no
 * pixel/render test on `MessageScreenshotView` would catch (that only tests how
 * a given title draws). So the scope decision is pinned here on the pure seam.
 */
describe("snapshotStaticTaskRefs", () => {
  const message = (content: string): MessageDto =>
    ({ id: `m-${content.length}`, content, attachments: [] }) as unknown as MessageDto;

  const task = (overrides: Partial<TaskSummaryDto>): TaskSummaryDto =>
    ({
      id: "task-x",
      task_number: 1,
      title: "Untitled",
      status: "todo",
      ...overrides,
    }) as unknown as TaskSummaryDto;

  it("reads a WORKSPACE conversation from its workspace scope — never keyed by the conversation id", () => {
    const client = new QueryClient();
    const wsScope = taskBoardCacheScope({
      workspaceId: "ws-1",
      conversationId: "channel-7",
    })!;
    client.setQueryData(
      taskByNumberKey(wsScope, 42),
      task({ task_number: 42, title: "Ship the board" }),
    );
    // A decoy in the WRONG cell — if the seam keyed a workspace conversation on
    // its conversation id (the bug), it would read this instead of the board.
    const conversationMiskey = taskBoardCacheScope({
      workspaceId: null,
      conversationId: "channel-7",
    })!;
    client.setQueryData(
      taskByNumberKey(conversationMiskey, 42),
      task({ task_number: 42, title: "WRONG CELL" }),
    );

    const snapshot = snapshotStaticTaskRefs(
      [message("see #task:42")],
      { workspaceId: "ws-1", conversationId: "channel-7" },
      client,
    );

    expect(snapshot.get(42)).toEqual({
      task_number: 42,
      title: "Ship the board",
      status: "todo",
    });
  });

  it("reads a DM (workspaceId null) from its pair scope, not a dropped title", () => {
    const client = new QueryClient();
    const pairScope = taskBoardCacheScope({
      workspaceId: null,
      conversationId: "pair-9",
    })!;
    client.setQueryData(
      taskByNumberKey(pairScope, 7),
      task({ id: "task-7", task_number: 7, title: "DM task" }),
    );

    const snapshot = snapshotStaticTaskRefs(
      [message("re #task:7")],
      { workspaceId: null, conversationId: "pair-9" },
      client,
    );

    // The old workspace-only chain projected a null scope for a DM and dropped
    // every title; the pair scope keeps it.
    expect(snapshot.get(7)).toEqual({ task_number: 7, title: "DM task", status: "todo" });
  });

  it("snapshots nothing for a coordinate that names no board — a SEAM property, unreachable from the modal", () => {
    // `TaskRefCoord.conversationId` is nullable because the coordinate type is
    // shared, so the seam owes that input an answer: no board ⇒ nothing frozen,
    // rather than a scope guessed from the workspace half. The modal itself cannot
    // reach this arm — its `conversationId` prop is `string` (#task:2041 B3), which
    // is what keeps a silent no-scope export out of production.
    const client = new QueryClient();
    const snapshot = snapshotStaticTaskRefs(
      [message("orphan #task:1")],
      { workspaceId: null, conversationId: null },
      client,
    );
    expect(snapshot.size).toBe(0);
  });

  it("omits a task that has not resolved in cache — a number without a title stays bare", () => {
    const client = new QueryClient();
    const snapshot = snapshotStaticTaskRefs(
      [message("#task:99 not cached")],
      { workspaceId: "ws-1", conversationId: "channel-7" },
      client,
    );
    expect(snapshot.has(99)).toBe(false);
  });
});
