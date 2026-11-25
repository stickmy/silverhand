import { beforeEach, describe, expect, it, vi } from "vitest";

const store = new Map<string, string>();
(globalThis as { localStorage: Pick<Storage, "getItem" | "setItem" | "removeItem"> }).localStorage =
  {
    getItem: (key) => store.get(key) ?? null,
    setItem: (key, value) => void store.set(key, String(value)),
    removeItem: (key) => void store.delete(key),
  };

const { PANE_MEMORY_KEY, clearPaneMemory, rememberPanes, rememberedPanes } =
  await import("./pane-memory");

describe("pane memory", () => {
  beforeEach(() => {
    clearPaneMemory();
    store.clear();
  });

  it("reads back recorded panes in order", () => {
    const panes = [
      { kind: "files", of: "ch-A", attachment: null },
      { kind: "threads", of: "ch-A", thread: "m-1" },
    ] as const;
    rememberPanes("ch-A", panes);
    expect(rememberedPanes("ch-A")).toEqual(panes);
    expect(rememberedPanes("nope")).toEqual([]);
  });

  it("keeps a deliberate close", () => {
    rememberPanes("ch-A", [{ kind: "threads", of: "ch-A", thread: null }]);
    rememberPanes("ch-A", []);
    expect(rememberedPanes("ch-A")).toEqual([]);
    expect(store.get(PANE_MEMORY_KEY)).toBe(JSON.stringify({ "ch-A": "" }));
  });

  it("clears memory and storage", () => {
    rememberPanes("ch-A", [{ kind: "threads", of: "ch-A", thread: "m-1" }]);
    clearPaneMemory();
    expect(rememberedPanes("ch-A")).toEqual([]);
    expect(store.has(PANE_MEMORY_KEY)).toBe(false);
  });

  it("hydrates from storage, dropping malformed entries through the URL parser", async () => {
    store.set(PANE_MEMORY_KEY, JSON.stringify({ "ch-A": "pane=wat&pane=threads.m-1", bad: 3 }));
    vi.resetModules();
    const fresh = await import("./pane-memory");
    expect(fresh.rememberedPanes("ch-A")).toEqual([{ kind: "threads", of: "ch-A", thread: "m-1" }]);
    expect(fresh.rememberedPanes("bad")).toEqual([]);
    vi.resetModules();
  });
});
