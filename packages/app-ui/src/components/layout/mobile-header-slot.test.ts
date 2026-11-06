import { expect, it, vi } from "vitest";
const h = vi.hoisted(() => ({
  state: undefined as any,
  context: null as any,
  section: "tasks",
  nextContext: 0,
}));
vi.mock("react", async (original) => ({
  ...(await original<typeof import("react")>()),
  useState: (initial: unknown) => [
    (h.state ??= initial),
    (value: any) => {
      h.state = typeof value === "function" ? value(h.state) : value;
    },
  ],
  useContext: (context: any) =>
    context._currentValue === null && h.nextContext++ % 2 === 0 ? h.context : h.section,
  useRef: (value: unknown) => ({ current: value }),
  useCallback: (callback: unknown) => callback,
  useMemo: (factory: () => unknown) => factory(),
}));
vi.mock("@/navigation/layout", () => ({ useCompactLayout: () => true }));
import { MobileHeaderProvider, MobileNavigationSlot } from "./mobile-header-slot";
import type { AppSection } from "./nav-rail";

function provider(section: AppSection) {
  const value = MobileHeaderProvider({ section, children: null }).props.value;
  h.context = value;
  return value.target;
}
function slot(section: AppSection) {
  h.section = section;
  h.nextContext = 0;
  return (MobileNavigationSlot() as any).ref as (node: HTMLSpanElement | null) => void;
}
it("unregisters only the old owner and ignores late registrations from inactive sections", () => {
  h.state = undefined;
  provider("tasks");
  const old = slot("tasks");
  const current = slot("chat");
  const oldNode = {} as HTMLSpanElement;
  const node = {} as HTMLSpanElement;
  old(oldNode);
  current(node);
  expect(provider("chat")).toBe(node);
  old(null);
  expect(provider("chat")).toBe(node);
  const hidden = slot("tasks");
  hidden(oldNode);
  expect(provider("chat")).toBe(node);
  current(null);
  expect(provider("chat")).toBeNull();
  expect(provider("tasks")).toBe(oldNode);
});

it("keeps the new page in the same section when the prior page unmounts late", () => {
  h.state = undefined;
  provider("chat");
  const old = slot("chat");
  const current = slot("chat");
  const node = {} as HTMLSpanElement;
  old({} as HTMLSpanElement);
  current(node);
  old(null);
  expect(provider("chat")).toBe(node);
  current(null);
  expect(provider("chat")).toBeNull();
});
