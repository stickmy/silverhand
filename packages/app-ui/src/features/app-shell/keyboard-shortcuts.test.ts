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
    agentPanelId: null,
    filesListOpen: false,
    taskPaneOpen: false,
    sideConversationId: null,
    threadRootId: null,
    threadsListOpen: false,
    activityChannelOpen: false,
  };

  it("layer 1: ?agent is the topmost overlay — peels before every other pane", () => {
    expect(
      resolveContextualClose({
        ...NOTHING_OPEN,
        agentPanelId: "a1",
        filesListOpen: true,
        taskPaneOpen: true,
        threadRootId: "root1",
        activityChannelOpen: true,
      }),
    ).toEqual({ outcome: "closed-panel", target: "agent" });
  });

  it("layer 2: ?files peels above the task pane and secondary pane", () => {
    expect(
      resolveContextualClose({
        ...NOTHING_OPEN,
        filesListOpen: true,
        taskPaneOpen: true,
        threadRootId: "root1",
        sideConversationId: "c1",
      }),
    ).toEqual({ outcome: "closed-panel", target: "files" });
  });

  it("layer 3: the ?task pane peels above side/thread/threads (files already closed)", () => {
    // task overlays thread/threads (it preserves them as the restore stack), so
    // ⌘W peels the task pane before the conversation pane beneath it.
    expect(
      resolveContextualClose({
        ...NOTHING_OPEN,
        taskPaneOpen: true,
        threadRootId: "root1",
        threadsListOpen: true,
      }),
    ).toEqual({ outcome: "closed-panel", target: "task" });
  });

  it("layer 4: secondary pane resolves side > thread > threads (render-layer priority)", () => {
    // All three set (malformed URL) → side wins, matching layoutMemoryFromRoute.
    expect(
      resolveContextualClose({
        ...NOTHING_OPEN,
        sideConversationId: "c1",
        threadRootId: "root1",
        threadsListOpen: true,
      }),
    ).toEqual({ outcome: "closed-panel", target: "side" });
    // thread wins over threads-list
    expect(
      resolveContextualClose({
        ...NOTHING_OPEN,
        threadRootId: "root1",
        threadsListOpen: true,
      }),
    ).toEqual({ outcome: "closed-panel", target: "thread" });
    // threads-list alone
    expect(resolveContextualClose({ ...NOTHING_OPEN, threadsListOpen: true })).toEqual({
      outcome: "closed-panel",
      target: "threads",
    });
  });

  it("layer 5: an Activity channel detail is peeled only after every overlay above it", () => {
    // thread sits above the activity channel → peel the thread first
    expect(
      resolveContextualClose({
        ...NOTHING_OPEN,
        activityChannelOpen: true,
        threadRootId: "root1",
      }),
    ).toEqual({ outcome: "closed-panel", target: "thread" });
    // nothing above it → pop back to the Activity list
    expect(resolveContextualClose({ ...NOTHING_OPEN, activityChannelOpen: true })).toEqual({
      outcome: "closed-panel",
      target: "activity-channel",
    });
  });

  it("layer 6: nothing to peel → background the app (macOS Hide)", () => {
    // Also covers the Chat main channel: it never sets activityChannelOpen, so
    // when only it remains ⌘W falls through to backgrounding, not a close.
    expect(resolveContextualClose(NOTHING_OPEN)).toEqual({
      outcome: "background-app",
      target: null,
    });
  });

  it("one layer per press: files closes before the underlying thread", () => {
    const first = resolveContextualClose({
      ...NOTHING_OPEN,
      filesListOpen: true,
      threadRootId: "root1",
    });
    expect(first).toEqual({ outcome: "closed-panel", target: "files" });
    // next press (files now closed) peels the thread beneath it
    const second = resolveContextualClose({
      ...NOTHING_OPEN,
      filesListOpen: false,
      threadRootId: "root1",
    });
    expect(second).toEqual({ outcome: "closed-panel", target: "thread" });
  });
});

it("closes a human inspector before its preserved thread", () => {
  expect(
    resolveContextualClose({
      agentPanelId: null,
      humanPanelId: "user-b",
      filesListOpen: false,
      taskPaneOpen: false,
      sideConversationId: null,
      threadRootId: "root",
      threadsListOpen: false,
      activityChannelOpen: false,
    }),
  ).toEqual({ outcome: "closed-panel", target: "human" });
});

it("closes channel settings before the preserved thread", () => {
  expect(
    resolveContextualClose({
      agentPanelId: null,
      humanPanelId: null,
      channelSettingsId: "channel-a",
      filesListOpen: false,
      taskPaneOpen: false,
      sideConversationId: null,
      threadRootId: "root",
      threadsListOpen: false,
      activityChannelOpen: false,
    }),
  ).toEqual({ outcome: "closed-panel", target: "settings" });
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
