import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { MutationObserver, QueryClient, type MutationObserverOptions } from "@tanstack/react-query";
import { useAssignTask, useUpdateTaskStatus } from "./hooks";

const harness = vi.hoisted(() => ({
  qc: null as QueryClient | null,
  api: vi.fn(),
  begin: vi.fn(),
  revert: vi.fn(),
  reconcile: vi.fn(),
  restart: vi.fn(),
  close: vi.fn(),
  points: vi.fn(),
}));
vi.mock("@tanstack/react-query", async (original) => ({
  ...(await original<typeof import("@tanstack/react-query")>()),
  useQueryClient: () => harness.qc,
  useMutation: (options: MutationObserverOptions<unknown, Error, unknown, unknown>) => {
    const observer = new MutationObserver(harness.qc!, options);
    return { mutateAsync: (vars: unknown) => observer.mutate(vars) };
  },
}));
vi.mock("@/lib/api", async (original) => ({
  ...(await original<typeof import("@/lib/api")>()),
  api: (...args: unknown[]) => harness.api(...args),
}));
vi.mock("@/lib/toast", () => ({ toast: { error: vi.fn() } }));
vi.mock("../model/task-projection", async (original) => ({
  ...(await original<typeof import("../model/task-projection")>()),
  beginOptimisticStatusWrite: (...args: unknown[]) => harness.begin(...args),
  revertOptimisticStatusWrite: (...args: unknown[]) => harness.revert(...args),
  reconcileTaskStatusWrite: (...args: unknown[]) => harness.reconcile(...args),
  restartRacingTaskReads: (...args: unknown[]) => harness.restart(...args),
  closeOptimisticStatusWrite: (...args: unknown[]) => harness.close(...args),
  invalidateTaskPointCaches: (...args: unknown[]) => harness.points(...args),
}));

const vars = { taskId: "task-a", status: "in_progress" as const };
const write = { taskId: "task-a", projection: {}, closedAtUnknown: false };
beforeEach(() => {
  vi.resetAllMocks();
  harness.qc = new QueryClient({ defaultOptions: { mutations: { retry: false } } });
  harness.api.mockResolvedValue({ success: true });
  harness.begin.mockReturnValue(write);
});
afterEach(() => harness.qc?.clear());

describe("task mutation lifecycle", () => {
  it("status success begins, reconciles and closes the same write without root invalidation", async () => {
    const invalidate = vi.spyOn(harness.qc!, "invalidateQueries");
    await useUpdateTaskStatus().mutateAsync(vars);
    expect(harness.begin).toHaveBeenCalledExactlyOnceWith(harness.qc, vars);
    expect(harness.api).toHaveBeenCalledExactlyOnceWith("/api/tasks/task-a/status", {
      method: "POST",
      body: JSON.stringify({ status: "in_progress" }),
    });
    expect(harness.restart).toHaveBeenCalledExactlyOnceWith(harness.qc, "task-a");
    expect(harness.reconcile).toHaveBeenCalledExactlyOnceWith(harness.qc, write.projection, {
      detail: false,
    });
    expect(harness.close).toHaveBeenCalledExactlyOnceWith(write);
    expect(harness.revert).not.toHaveBeenCalled();
    expect(invalidate).not.toHaveBeenCalled();
    expect(harness.begin.mock.invocationCallOrder[0]).toBeLessThan(
      harness.api.mock.invocationCallOrder[0]!,
    );
    expect(harness.reconcile.mock.invocationCallOrder[0]).toBeLessThan(
      harness.close.mock.invocationCallOrder[0]!,
    );
  });

  it("status failure rolls back, re-reads detail and chips, and closes the ledger", async () => {
    const error = new Error("offline");
    harness.api.mockRejectedValue(error);
    await expect(useUpdateTaskStatus().mutateAsync(vars)).rejects.toBe(error);
    expect(harness.revert).toHaveBeenCalledExactlyOnceWith(harness.qc, write);
    expect(harness.reconcile).toHaveBeenCalledExactlyOnceWith(harness.qc, write.projection, {
      detail: true,
      points: true,
    });
    expect(harness.close).toHaveBeenCalledExactlyOnceWith(write);
    expect(harness.restart).not.toHaveBeenCalled();
  });

  it("does not roll back or close a write that never began", async () => {
    harness.begin.mockImplementation(() => {
      throw new Error("begin failed");
    });
    await expect(useUpdateTaskStatus().mutateAsync(vars)).rejects.toThrow("begin failed");
    expect(harness.api).not.toHaveBeenCalled();
    expect(harness.revert).not.toHaveBeenCalled();
    expect(harness.close).not.toHaveBeenCalled();
  });

  it.each(["agent-a", null])(
    "assignment %s invalidates boards but scopes detail/chips to the task",
    async (agentId) => {
      const qc = harness.qc!;
      const keys = [
        ["tasks", "board", "facets"],
        ["task-detail", "task-a"],
        ["task-detail", "other"],
        ["task-by-number", "other"],
      ];
      for (const key of keys) qc.setQueryData(key, "cached");
      await useAssignTask().mutateAsync({ taskId: "task-a", agentId });
      expect(qc.getQueryState(keys[0]!)?.isInvalidated).toBe(true);
      expect(qc.getQueryState(keys[1]!)?.isInvalidated).toBe(true);
      expect(qc.getQueryState(keys[2]!)?.isInvalidated).toBe(false);
      expect(qc.getQueryState(keys[3]!)?.isInvalidated).toBe(false);
      expect(harness.points).toHaveBeenCalledExactlyOnceWith(qc, "task-a");
      expect(harness.api.mock.calls[0]?.[0]).toBe(
        `/api/tasks/task-a/${agentId ? "claim" : "unclaim"}`,
      );
    },
  );
});
