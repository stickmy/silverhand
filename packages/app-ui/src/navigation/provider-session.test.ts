import { afterEach, expect, it, vi } from "vitest";

const h = vi.hoisted(() => ({ effects: [] as Array<() => void | (() => void)> }));
vi.mock("react", async (original) => ({
  ...(await original<typeof import("react")>()),
  useState: (init: unknown) => [typeof init === "function" ? init() : init, vi.fn()],
  useMemo: (fn: () => unknown) => fn(),
  useCallback: (fn: unknown) => fn,
  useRef: (value: unknown) => ({ current: value }),
  useEffect: (fn: () => void | (() => void)) => h.effects.push(fn),
  useSyncExternalStore: (_subscribe: unknown, get: () => unknown) => get(),
}));
vi.mock("./layout", () => ({ useCompactLayout: () => true }));
vi.mock("@/platform", () => ({ getPlatform: () => ({}) }));
import { NavigationLaunch } from "./launch";
import { NavigationProvider } from "./provider";

afterEach(() => vi.unstubAllGlobals());
function mountWindow(hash: string) {
  const location = { hash };
  vi.stubGlobal("window", {
    location,
    history: {
      replaceState: (_state: unknown, _title: string, url: string) => {
        location.hash = url;
      },
    },
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
  });
  return location;
}
function render(launch: NavigationLaunch) {
  h.effects = [];
  const tree = NavigationProvider({ children: null, launch });
  return { location: tree.props.children.props.children.props.location, setup: h.effects.at(-1)! };
}
it("new authentication cannot read the previous domain's hash before its cleanup", () => {
  const address = mountWindow("#/chat/d/private-old?settings=private-old");
  const launch = new NavigationLaunch(address.hash.slice(1));
  const old = render(launch);
  const cleanupOld = old.setup()!;
  // The App-owned launch survives keyed domain replacement.
  const replacement = render(launch);
  cleanupOld();
  const cleanupNew = replacement.setup()!;
  expect(replacement.location.pathname).toBe("/chat");
  expect(address.hash).toBe("#/chat");
  cleanupNew();
});
it("logout then login starts at root even if the address changed while signed out", () => {
  const address = mountWindow("#/chat/d/old");
  const launch = new NavigationLaunch("/chat/d/old");
  render(launch).setup()!();
  address.hash = "#/chat/d/old?thread=old";
  const next = render(launch);
  const cleanup = next.setup()!;
  expect(next.location.pathname).toBe("/chat");
  expect(address.hash).toBe("#/chat");
  cleanup();
});
it("initial deep link survives StrictMode effect cleanup and replay", () => {
  const url = "/chat/d/initial?settings=initial&settingsPage=members";
  const address = mountWindow(`#${url}`);
  const first = render(new NavigationLaunch(url));
  const cleanup = first.setup()!;
  cleanup();
  expect(address.hash).toBe("#/chat");
  const cleanupReplay = first.setup()!;
  expect(address.hash).toBe(`#${url}`);
  expect(first.location.pathname + first.location.search).toBe(url);
  cleanupReplay();
});

it("profile setup and abandoned renders do not consume the first launch target", () => {
  const launch = new NavigationLaunch("/chat/d/first");
  mountWindow("#/chat/d/first");
  render(launch);
  const committed = render(launch);
  expect(committed.location.pathname).toBe("/chat/d/first");
  committed.setup()!();
  expect(launch.initialUrl).toBe("/chat");
});
