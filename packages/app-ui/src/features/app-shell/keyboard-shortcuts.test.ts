import { describe, expect, it, vi } from "vitest";
import {
  NAV_SECTION_SHORTCUTS,
  dispatchNavSection,
  resolveContextualClose,
  tabCycleStep,
  type ContextualCloseState,
  type NavSectionApi,
} from "./keyboard-shortcuts";

/**
 * Behavior-level tests for the two keyboard-shortcut cores: the
 * ⌘1–5 nav mapping and the ⌘W ladder resolver. Both are pure by design, so the
 * contract that matters — WHICH section a digit selects, and WHICH layer ⌘W
 * peels — is locked here without a DOM/keyboard harness. Per standing rule this
 * guards SEMANTICS (mapping + ladder order), not sizes or key-event plumbing.
 */

describe("NAV_SECTION_SHORTCUTS — ⌘1–5 → nav-rail section", () => {
  it("binds digits 1–5 to the five primary sections in rail order", () => {
    expect(NAV_SECTION_SHORTCUTS.map((s) => [s.digit, s.section])).toEqual([
      [1, "chat"],
      [2, "activity"],
      [3, "tasks"],
      [4, "members"],
      [5, "computers"],
    ]);
  });

  it("keys on the physical Digit<n> code so the binding is layout-stable", () => {
    for (const s of NAV_SECTION_SHORTCUTS) {
      expect(s.code).toBe(`Digit${s.digit}`);
    }
  });

  it("never binds settings (bottom utility, no digit)", () => {
    expect(NAV_SECTION_SHORTCUTS.some((s) => (s.section as string) === "settings")).toBe(false);
  });
});

describe("dispatchNavSection — routes a section to its nav call", () => {
  function makeNav(): NavSectionApi & Record<string, ReturnType<typeof vi.fn>> {
    return {
      toChat: vi.fn(),
      toActivity: vi.fn(),
      toTasks: vi.fn(),
      toMembers: vi.fn(),
      toComputers: vi.fn(),
    };
  }

  it.each([
    ["chat", "toChat"],
    ["activity", "toActivity"],
    ["tasks", "toTasks"],
    ["members", "toMembers"],
    ["computers", "toComputers"],
  ] as const)("%s → nav.%s()", (section, method) => {
    const nav = makeNav();
    dispatchNavSection(nav, section);
    expect(nav[method]).toHaveBeenCalledTimes(1);
    // no other nav call fired
    for (const [key, fn] of Object.entries(nav)) {
      if (key !== method) expect(fn).not.toHaveBeenCalled();
    }
  });
});

describe("resolveContextualClose — the ⌘W ladder", () => {
  const NOTHING_OPEN: ContextualCloseState = {
    panes: [],
    taskPaneOpen: false,
    activityChannelOpen: false,
  };

  it("closes the last pane, whatever its kind", () => {
    const panes: ContextualCloseState["panes"] = [
      { kind: "threads", of: "c", thread: "root" },
      { kind: "conversation", id: "c2" },
      { kind: "agent", id: "a1" },
    ];
    expect(resolveContextualClose({ ...NOTHING_OPEN, panes, activityChannelOpen: true })).toEqual({
      outcome: "closed-panel",
      target: { pane: panes[2] },
    });
    expect(resolveContextualClose({ ...NOTHING_OPEN, panes: panes.slice(0, 2) })).toEqual({
      outcome: "closed-panel",
      target: { pane: panes[1] },
    });
  });

  it("skips a task shown as the modal, which owns its own dismissal", () => {
    const panes: ContextualCloseState["panes"] = [
      { kind: "files", of: "c", attachment: null },
      { kind: "task", id: "t1" },
    ];
    expect(resolveContextualClose({ ...NOTHING_OPEN, panes })).toEqual({
      outcome: "closed-panel",
      target: { pane: panes[0] },
    });
    expect(resolveContextualClose({ ...NOTHING_OPEN, panes, taskPaneOpen: true })).toEqual({
      outcome: "closed-panel",
      target: { pane: panes[1] },
    });
  });

  it("pops an Activity channel detail only after every pane is closed", () => {
    expect(resolveContextualClose({ ...NOTHING_OPEN, activityChannelOpen: true })).toEqual({
      outcome: "closed-panel",
      target: "activity-channel",
    });
  });

  it("with nothing to close, backgrounds the app (macOS Hide)", () => {
    // Also covers the Chat main channel: it never sets activityChannelOpen, so
    // when only it remains ⌘W falls through to backgrounding, not a close.
    expect(resolveContextualClose(NOTHING_OPEN)).toEqual({
      outcome: "background-app",
      target: null,
    });
  });
});

describe("tabCycleStep", () => {
  const key = (over: Partial<Parameters<typeof tabCycleStep>[0]>) => ({
    key: "",
    code: "",
    metaKey: false,
    ctrlKey: false,
    altKey: false,
    shiftKey: false,
    ...over,
  });

  it("maps Tab, ⌃Tab and ⌘⇧] forward, their shifted twins back", () => {
    expect(tabCycleStep(key({ key: "Tab" }))).toBe(1);
    expect(tabCycleStep(key({ key: "Tab", shiftKey: true }))).toBe(-1);
    expect(tabCycleStep(key({ key: "Tab", ctrlKey: true }))).toBe(1);
    expect(tabCycleStep(key({ key: "Tab", ctrlKey: true, shiftKey: true }))).toBe(-1);
    expect(
      tabCycleStep(key({ key: "}", code: "BracketRight", metaKey: true, shiftKey: true })),
    ).toBe(1);
    expect(
      tabCycleStep(key({ key: "{", code: "BracketLeft", metaKey: true, shiftKey: true })),
    ).toBe(-1);
  });

  it("leaves other chords alone", () => {
    expect(tabCycleStep(key({ key: "Tab", metaKey: true }))).toBe(0);
    expect(tabCycleStep(key({ key: "Tab", ctrlKey: true, altKey: true }))).toBe(0);
    expect(tabCycleStep(key({ key: "]", code: "BracketRight", metaKey: true }))).toBe(0);
  });
});
