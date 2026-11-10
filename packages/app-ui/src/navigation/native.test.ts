import { modalScope } from "@/lib/modal-scope";
import { afterEach, expect, it, vi } from "vitest";
const h = vi.hoisted(() => ({
  effects: [] as Array<() => void | (() => void)>,
  native: undefined as any,
}));
vi.mock("react", () => ({
  useEffect: (fn: () => void) => h.effects.push(fn),
  useCallback: (fn: unknown) => fn,
}));
vi.mock("./layout", () => ({ useCompactLayout: () => true }));
vi.mock("@/platform", () => ({ getPlatform: () => ({ navigation: h.native }) }));
import { useNativeNavigation } from "./native";
import { NavigationStore } from "./store";
import { showsTabBar } from "./presentation";
afterEach(() => {
  h.effects = [];
  vi.unstubAllGlobals();
});
function setup() {
  let handler: (intent: any) => void = () => {};
  h.native = {
    sync: vi.fn(async () => ({ tabBarHeight: 80 })),
    reset: vi.fn(async () => {}),
    subscribe: vi.fn(async (fn) => {
      handler = fn;
      return vi.fn();
    }),
  };
  vi.stubGlobal("document", {
    documentElement: { style: { setProperty: vi.fn(), removeProperty: vi.fn() } },
  });
  const store = new NavigationStore("/chat");
  let allowed = true;
  const run = useNativeNavigation(store, () => allowed, false);
  const cleanup = h.effects[0]!()!;
  return {
    store,
    run,
    cleanup,
    deny: () => {
      allowed = false;
    },
    emit: (target: string, modalVersion = modalScope.version) =>
      handler({ session: store.session, modalVersion, kind: "tab", target }),
  };
}
it("hides global navigation in details, retains workspace and filtered task roots", () => {
  expect(showsTabBar("/chat/w/workspace")).toBe(true);
  expect(showsTabBar("/me")).toBe(true);
  expect(showsTabBar("/tasks", "?status=open")).toBe(true);
  for (const path of ["/chat/d/c", "/members", "/settings"]) expect(showsTabBar(path)).toBe(false);
  expect(showsTabBar("/tasks", "?task=t")).toBe(false);
});

it("commits at once, and not at all while a leave guard holds", () => {
  const { store, run, deny, cleanup } = setup();
  expect(run(() => store.go("/chat/d/c"))).toBe(true);
  expect(store.getSnapshot().location.pathname).toBe("/chat/d/c");
  deny();
  expect(run(store.back)).toBe(false);
  expect(store.getSnapshot().location.pathname).toBe("/chat/d/c");
  cleanup();
});

it("syncs the entry on screen on every change, without the page Back returns to", async () => {
  const { store, cleanup } = setup();
  store.go("/chat/d/c");
  await vi.waitFor(() =>
    expect(h.native.sync.mock.calls.at(-1)[0]).toEqual({
      session: store.session,
      key: store.getSnapshot().location.key,
      tab: "chat",
      blocked: false,
      modalVersion: modalScope.version,
      visible: false,
      dark: false,
    }),
  );
  store.back();
  await vi.waitFor(() => expect(h.native.sync.mock.calls.at(-1)[0].visible).toBe(true));
  cleanup();
});

it("selects a tab from native, unless a modal is open or has changed since", () => {
  const { store, emit, cleanup } = setup();
  emit("tasks");
  expect(store.getSnapshot().tab).toBe("tasks");
  const close = modalScope.acquire();
  emit("activity");
  expect(store.getSnapshot().tab).toBe("tasks");
  const version = modalScope.version;
  close();
  emit("activity", version);
  expect(store.getSnapshot().tab).toBe("tasks");
  emit("nowhere");
  expect(store.getSnapshot().tab).toBe("tasks");
  emit("activity");
  expect(store.getSnapshot().tab).toBe("activity");
  cleanup();
});

it("puts native's tab back when the page refuses the selection", async () => {
  const { store, emit, deny, cleanup } = setup();
  await vi.waitFor(() => expect(h.native.sync).toHaveBeenCalled());
  deny();
  h.native.sync.mockClear();
  emit("tasks");
  expect(store.getSnapshot().tab).toBe("chat");
  await vi.waitFor(() => expect(h.native.sync.mock.calls.at(-1)?.[0].tab).toBe("chat"));
  cleanup();
});

it("reports a modal to native, and allows a modal's own navigation", async () => {
  const { store, run, cleanup } = setup();
  const close = modalScope.acquire();
  expect(h.native.sync.mock.calls.at(-1)[0].blocked).toBe(true);
  expect(run(() => store.go("/tasks?task=created"))).toBe(true);
  expect(store.getSnapshot().location.search).toBe("?task=created");
  close();
  expect(h.native.sync.mock.calls.at(-1)[0].blocked).toBe(false);
  cleanup();
});

it("pages pushed from Me keep Me selected, and a Me tab intent restores its stack", async () => {
  const { store, cleanup } = setup();
  store.selectTab("me");
  for (const path of ["/members", "/computers", "/settings"]) {
    store.go(path);
    await vi.waitFor(() => expect(h.native.sync.mock.calls.at(-1)[0].visible).toBe(false));
    expect(h.native.sync.mock.calls.at(-1)[0].tab).toBe("me");
    store.back();
    expect(store.getSnapshot().location.pathname).toBe("/me");
    await vi.waitFor(() => expect(h.native.sync.mock.calls.at(-1)[0].visible).toBe(true));
  }
  cleanup();
});

it("resets native and drops the tab bar height when the session ends", () => {
  const { store, cleanup } = setup();
  cleanup();
  expect(h.native.reset).toHaveBeenCalledWith(store.session);
  expect(document.documentElement.style.removeProperty).toHaveBeenCalledWith(
    "--native-tab-bar-height",
  );
});
