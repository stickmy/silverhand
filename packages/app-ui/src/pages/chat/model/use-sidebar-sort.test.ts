import { afterEach, describe, expect, it, vi } from "vitest";
import { QueryClient } from "@tanstack/react-query";
import { apiPaths, SIDEBAR_FIXED_BRANCH_IDS, SIDEBAR_SORT_CHANGED_EVENT } from "@silverhand/shared";
import { useSidebarSort } from "./use-sidebar-sort";

const h = vi.hoisted(() => ({
  effects: [] as Array<() => () => void>,
  listener: null as null | ((frame: unknown) => void),
  request: vi.fn(),
  setPending: vi.fn(),
  toast: vi.fn(),
  qc: null as QueryClient | null,
  data: null as unknown,
}));
vi.mock("react", () => ({
  useEffect: (effect: () => () => void) => h.effects.push(effect),
  useMemo: (factory: () => unknown) => factory(),
  useRef: (current: unknown) => ({ current }),
  useState: () => [null, h.setPending],
}));
vi.mock("@tanstack/react-query", async (original) => ({
  ...(await original<typeof import("@tanstack/react-query")>()),
  useQueryClient: () => h.qc,
  useQuery: () => ({ data: h.data }),
}));
vi.mock("@/features/session/hooks", () => ({ useViewerUser: () => ({ data: { id: "current" } }) }));
vi.mock("@/lib/api", () => ({ api: (...args: unknown[]) => h.request(...args) }));
vi.mock("@/lib/toast", () => ({ toast: { error: (...args: unknown[]) => h.toast(...args) } }));
vi.mock("@/lib/ws-client", () => ({
  subscribeViewerScope: () => ({ unsubscribe() {} }),
  isCurrentViewerScope: (scope: string) => scope === "viewer:current",
  onWsFrame: (listener: (frame: unknown) => void) => {
    h.listener = listener;
    return () => {
      h.listener = null;
    };
  },
}));
const key = ["workspaces", "sidebar-sort", "current"];
const ids = [SIDEBAR_FIXED_BRANCH_IDS[0], "workspace:one", SIDEBAR_FIXED_BRANCH_IDS[1]];
const response = {
  viewer_sort: {
    list_key: "sidebar",
    modes: { "": "manual" },
    entries: ids.map((entity_id, rank) => ({
      entity_id,
      rank,
      starred_at: null,
      last_accessed_at: null,
    })),
  },
};
function setup() {
  h.qc = new QueryClient();
  h.qc.setQueryData(key, response);
  h.data = response;
  const invalidate = vi.spyOn(h.qc, "invalidateQueries").mockResolvedValue();
  const model = useSidebarSort();
  const cleanup = h.effects[0]!();
  return { model, cleanup, invalidate };
}
afterEach(() => {
  h.qc?.clear();
  h.effects = [];
  h.listener = null;
  vi.clearAllMocks();
});
describe("sidebar sort persistence", () => {
  it("leaves ordering to the fallback when the read is unavailable and does not submit an incomplete set", async () => {
    const { cleanup } = setup();
    h.data = undefined;
    const model = useSidebarSort();
    expect(model.order).toBeUndefined();
    expect(model.ready).toBe(false);
    await model.reorder(ids);
    expect(h.request).not.toHaveBeenCalled();
    cleanup();
  });

  it("sends the complete mixed order including empty fixed sections", async () => {
    const { model, invalidate, cleanup } = setup();
    h.request.mockResolvedValue(response);
    const order = [...ids].reverse();
    await model.reorder(order);
    expect(h.request.mock.calls[0]?.[0]).toBe(apiPaths.sortOrder);
    expect(JSON.parse(h.request.mock.calls[0]?.[1].body)).toEqual({
      list_key: "sidebar",
      group: "sidebar",
      ordered_ids: order,
    });
    expect(h.setPending.mock.calls[0]).toEqual([{ viewerId: "current", ids: order }]);
    expect(h.setPending).toHaveBeenLastCalledWith(null);
    expect(invalidate).toHaveBeenCalledOnce();
    cleanup();
  });
  it("rereads after 409 and clears the optimistic order", async () => {
    const { model, invalidate, cleanup } = setup();
    h.request.mockRejectedValue({ status: 409 });
    await model.reorder([...ids].reverse());
    expect(invalidate).toHaveBeenCalledOnce();
    expect(h.setPending).toHaveBeenLastCalledWith(null);
    expect(h.qc!.getQueryData(key)).toEqual(response);
    cleanup();
  });
  it.each([SIDEBAR_SORT_CHANGED_EVENT, "viewer_workspaces_changed", "authenticated"])(
    "discards a late ACK after %s",
    async (kind) => {
      const { model, invalidate, cleanup } = setup();
      let resolve!: (value: unknown) => void;
      h.request.mockImplementation(
        () =>
          new Promise((done) => {
            resolve = done;
          }),
      );
      const pending = model.reorder([...ids].reverse());
      await vi.waitFor(() => expect(h.request).toHaveBeenCalledOnce());
      h.listener?.(
        kind === "authenticated"
          ? { type: kind }
          : { type: "event", scope: "viewer:current", kind },
      );
      await model.reorder(ids);
      expect(h.request).toHaveBeenCalledOnce();
      const newer = { ...response, viewer_sort: { ...response.viewer_sort, entries: [] } };
      h.qc!.setQueryData(key, newer);
      resolve(response);
      await pending;
      expect(h.qc!.getQueryData(key)).toEqual(newer);
      expect(h.setPending).toHaveBeenLastCalledWith(null);
      expect(invalidate).toHaveBeenCalledOnce();
      cleanup();
    },
  );
  it("does not write or refetch after the viewer's component is disposed", async () => {
    const { model, invalidate, cleanup } = setup();
    let resolve!: (value: unknown) => void;
    h.request.mockImplementation(
      () =>
        new Promise((done) => {
          resolve = done;
        }),
    );
    const pending = model.reorder(ids);
    await vi.waitFor(() => expect(h.request).toHaveBeenCalledOnce());
    cleanup();
    h.qc!.clear();
    resolve(response);
    await pending;
    expect(h.qc!.getQueryData(key)).toBeUndefined();
    expect(invalidate).not.toHaveBeenCalled();
  });
});
