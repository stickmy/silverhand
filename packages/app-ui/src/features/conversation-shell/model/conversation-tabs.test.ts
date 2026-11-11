import { describe, expect, it } from "vitest";
import {
  DEFAULT_TABS,
  addTab,
  adjacentTabKey,
  firstTabKey,
  isDefaultTabs,
  isRemovable,
  normalizeTabs,
  parseTab,
  parseTabs,
  removeTab,
  reorderTabs,
  resolveActiveTabKey,
  tabKey,
  type PanelTabKind,
  type Tab,
} from "./conversation-tabs";

const chat: Tab = { kind: "chat" };
const tasks: Tab = { kind: "tasks" };
const att = (id: string, filename = `${id}.md`, contentType = "text/markdown"): Tab => ({
  kind: "attachment",
  attachmentId: id,
  filename,
  contentType,
});
const panel = (kind: PanelTabKind): Tab => ({ kind: "panel", panel: kind });

describe("tabKey / firstTabKey / isRemovable", () => {
  it("keys builtins by kind and attachments by id namespace", () => {
    expect(tabKey(chat)).toBe("chat");
    expect(tabKey(tasks)).toBe("tasks");
    expect(tabKey(att("a"))).toBe("attachment:a");
  });

  it("keys a panel surface by its kind, in its own namespace", () => {
    expect(tabKey(panel("attachments"))).toBe("panel:attachments");
    expect(tabKey(panel("threads"))).toBe("panel:threads");
  });

  it("an attachment id that spells a builtin kind stays in its own namespace", () => {
    expect(tabKey(att("chat"))).toBe("attachment:chat");
  });

  it("firstTabKey follows order, not a hardcoded Chat", () => {
    expect(firstTabKey([tasks, chat, att("a")])).toBe("tasks");
    expect(firstTabKey([])).toBe("chat");
  });

  it("only custom tabs are removable", () => {
    expect(isRemovable(chat)).toBe(false);
    expect(isRemovable(tasks)).toBe(false);
    expect(isRemovable(att("a"))).toBe(true);
    expect(isRemovable(panel("threads"))).toBe(true);
  });
});

describe("normalizeTabs — builtins invariant", () => {
  it("appends missing builtins in chat-then-tasks order", () => {
    expect(normalizeTabs([]).map(tabKey)).toEqual(["chat", "tasks"]);
    expect(normalizeTabs([att("a")]).map(tabKey)).toEqual(["attachment:a", "chat", "tasks"]);
  });

  it("preserves an existing builtin order the viewer dragged", () => {
    expect(normalizeTabs([tasks, att("a"), chat]).map(tabKey)).toEqual([
      "tasks",
      "attachment:a",
      "chat",
    ]);
  });

  it("drops duplicate keys, first wins", () => {
    expect(normalizeTabs([chat, chat, att("a"), att("a", "a2.md")]).map(tabKey)).toEqual([
      "chat",
      "attachment:a",
      "tasks",
    ]);
    // first attachment 'a' kept its original filename
    const out = normalizeTabs([att("a", "first.md"), att("a", "second.md")]);
    expect(out.find((t) => tabKey(t) === "attachment:a")).toMatchObject({ filename: "first.md" });
  });
});

describe("addTab — open-or-focus dedupe", () => {
  it("appends a genuinely new tab at the end", () => {
    expect(addTab([chat, tasks], att("a")).map(tabKey)).toEqual(["chat", "tasks", "attachment:a"]);
  });

  it("re-adding an open attachment returns the SAME array (focus, no duplicate)", () => {
    const list = [chat, tasks, att("a")];
    const out = addTab(list, att("a", "renamed.md"));
    expect(out).toBe(list);
  });

  it("one tab per panel surface: re-adding an open surface focuses it", () => {
    const opened = addTab([chat, tasks], panel("threads"));
    expect(opened.map(tabKey)).toEqual(["chat", "tasks", "panel:threads"]);
    expect(addTab(opened, panel("threads"))).toBe(opened);
    expect(addTab(opened, panel("attachments")).map(tabKey)).toEqual([
      "chat",
      "tasks",
      "panel:threads",
      "panel:attachments",
    ]);
  });
});

describe("removeTab — builtins non-removable, customs non-destructive", () => {
  it("removes a custom tab by key", () => {
    expect(removeTab([chat, tasks, att("a")], "attachment:a").map(tabKey)).toEqual([
      "chat",
      "tasks",
    ]);
    expect(removeTab([chat, tasks, panel("threads")], "panel:threads").map(tabKey)).toEqual([
      "chat",
      "tasks",
    ]);
  });

  it("refuses to remove a builtin", () => {
    const list = [chat, tasks, att("a")];
    expect(removeTab(list, "chat")).toBe(list);
    expect(removeTab(list, "tasks")).toBe(list);
  });

  it("is a no-op for an absent key", () => {
    const list = [chat, tasks];
    expect(removeTab(list, "attachment:missing")).toBe(list);
  });
});

describe("reorderTabs — whole list, builtins included", () => {
  it("drags a builtin after a custom tab", () => {
    expect(reorderTabs([chat, tasks, att("a")], 0, 2).map(tabKey)).toEqual([
      "tasks",
      "attachment:a",
      "chat",
    ]);
  });

  it("returns unchanged for a no-op or out-of-range move", () => {
    const list = [chat, tasks];
    expect(reorderTabs(list, 1, 1)).toBe(list);
    expect(reorderTabs(list, -1, 0)).toBe(list);
    expect(reorderTabs(list, 0, 5)).toBe(list);
  });
});

describe("resolveActiveTabKey — fallback to first in order", () => {
  it("keeps the desired key when present", () => {
    const tabs = [tasks, chat, att("a")];
    expect(resolveActiveTabKey(tabs, "attachment:a")).toBe("attachment:a");
    expect(resolveActiveTabKey(tabs, "chat")).toBe("chat");
  });

  it("falls back to the first tab in order when desired is gone/absent", () => {
    expect(resolveActiveTabKey([tasks, chat], "attachment:gone")).toBe("tasks");
    expect(resolveActiveTabKey([tasks, chat], null)).toBe("tasks");
    expect(resolveActiveTabKey([tasks, chat], undefined)).toBe("tasks");
  });
});

describe("parseTab / parseTabs — reject non-conforming, repair builtins", () => {
  it("accepts well-formed builtins and attachments", () => {
    expect(parseTab({ kind: "chat" })).toEqual(chat);
    expect(parseTab({ kind: "tasks" })).toEqual(tasks);
    expect(
      parseTab({
        kind: "attachment",
        attachmentId: "a",
        filename: "a.md",
        contentType: "text/markdown",
      }),
    ).toEqual(att("a"));
  });

  it("accepts a known panel surface and drops an unknown one", () => {
    expect(parseTab({ kind: "panel", panel: "attachments" })).toEqual(panel("attachments"));
    expect(parseTab({ kind: "panel", panel: "threads" })).toEqual(panel("threads"));
    expect(parseTab({ kind: "panel", panel: "members" })).toBeNull();
    expect(parseTabs([{ kind: "panel", panel: "members" }]).map(tabKey)).toEqual(["chat", "tasks"]);
    // A surface this build no longer ships (or never shipped) must not hydrate
    // into a tab that mounts nothing.
    expect(parseTab({ kind: "panel", panel: "retired" })).toBeNull();
    expect(parseTab({ kind: "panel", panel: "toString" })).toBeNull();
  });

  it("drops malformed values", () => {
    expect(parseTab(null)).toBeNull();
    expect(parseTab({ kind: "panel" })).toBeNull();
    expect(
      parseTab({ kind: "attachment", attachmentId: "", filename: "x", contentType: "" }),
    ).toBeNull();
    expect(
      parseTab({ kind: "attachment", attachmentId: "a", filename: "", contentType: "" }),
    ).toBeNull();
    expect(
      parseTab({ kind: "attachment", attachmentId: "a", filename: "x", contentType: 42 }),
    ).toBeNull();
  });

  it("parseTabs keeps valid tabs and restores builtins", () => {
    expect(
      parseTabs([
        { kind: "attachment", attachmentId: "a", filename: "a.md", contentType: "" },
        { kind: "junk" },
      ]).map(tabKey),
    ).toEqual(["attachment:a", "chat", "tasks"]);
  });

  it("parseTabs round-trips a persisted panel order and dedupes repeats", () => {
    expect(
      parseTabs([
        { kind: "panel", panel: "threads" },
        { kind: "chat" },
        { kind: "panel", panel: "threads" },
      ]).map(tabKey),
    ).toEqual(["panel:threads", "chat", "tasks"]);
  });

  it("parseTabs yields the default order for a corrupt/non-array value", () => {
    expect(parseTabs(null).map(tabKey)).toEqual(["chat", "tasks"]);
    expect(parseTabs("nope").map(tabKey)).toEqual(["chat", "tasks"]);
    expect(parseTabs([]).map(tabKey)).toEqual(["chat", "tasks"]);
  });
});

describe("isDefaultTabs", () => {
  it("is true only for the untouched default order", () => {
    expect(isDefaultTabs([...DEFAULT_TABS])).toBe(true);
    expect(isDefaultTabs([chat, tasks, panel("threads"), panel("attachments")])).toBe(true);
    expect(isDefaultTabs([chat, tasks])).toBe(false); // closed default panels must persist
    expect(isDefaultTabs([tasks, chat, panel("threads"), panel("attachments")])).toBe(false);
    expect(isDefaultTabs([...DEFAULT_TABS, att("a")])).toBe(false);
  });
});

describe("adjacentTabKey", () => {
  const tabs = [chat, tasks, panel("threads")];

  it("steps forward and back in strip order, wrapping at both ends", () => {
    expect(adjacentTabKey(tabs, "chat", 1)).toBe("tasks");
    expect(adjacentTabKey(tabs, tabKey(panel("threads")), 1)).toBe("chat");
    expect(adjacentTabKey(tabs, "chat", -1)).toBe(tabKey(panel("threads")));
  });

  it("starts from the first tab when the active key is gone", () => {
    expect(adjacentTabKey(tabs, "missing", 1)).toBe("tasks");
  });
});
