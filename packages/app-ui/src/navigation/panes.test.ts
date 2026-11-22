import { describe, expect, it } from "vitest";
import {
  closePane,
  editPanesUrl,
  movePane,
  onScreen,
  openPane,
  parsePanes,
  settlePanes,
  togglePane,
  writePanes,
  type Panes,
} from "./panes";

const parse = (search: string, route: string | null = "c1"): Panes =>
  parsePanes(new URLSearchParams(search), route);
const write = (panes: Panes, search = ""): string => {
  const params = new URLSearchParams(search);
  writePanes(params, panes, "c1");
  return params.toString();
};

describe("panes URL", () => {
  it("round-trips the pane list in order and keeps unrelated params", () => {
    const search = "utm=1&pane=files.a1&pane=threads.r1&pane=settings.members&pane=task.t1";
    const panes = parse(search);
    expect(panes).toEqual([
      { kind: "files", of: "c1", attachment: "a1" },
      { kind: "threads", of: "c1", thread: "r1" },
      { kind: "settings", of: "c1", page: "members" },
      { kind: "task", id: "t1" },
    ]);
    expect(parse(write(panes, "utm=1"))).toEqual(panes);
    expect(write(panes, "utm=1").startsWith("utm=1&")).toBe(true);
  });

  it("drops bad entries, conversation panes, other conversations' panes and duplicate keys", () => {
    expect(parse("pane=nope&pane=conversation.c1&pane=threads~bad%2Fid&pane=agent.")).toEqual([]);
    expect(parse("pane=threads.r1&pane=threads.r2")).toEqual([
      { kind: "threads", of: "c1", thread: "r2" },
    ]);
    expect(parse("pane=files~c9")).toEqual([]);
    expect(parse("pane=conversation.c2&pane=threads.r1%7Ec2&pane=agent.a1")).toEqual([
      { kind: "agent", id: "a1" },
    ]);
  });

  it("rewrites only the pane list of a URL", () => {
    expect(editPanesUrl("/x", "?pane=threads.r1&utm=1", "c1", (p) => p)).toBe(
      "/x?utm=1&pane=threads.r1",
    );
  });
});

describe("pane edits", () => {
  const base: Panes = [
    { kind: "threads", of: "c1", thread: null },
    { kind: "agent", id: "a" },
    { kind: "files", of: "c1", attachment: null },
  ];

  it("opening a pane already open replaces it where it stands", () => {
    expect(openPane(base, { kind: "threads", of: "c1", thread: "r1" })[0]).toEqual({
      kind: "threads",
      of: "c1",
      thread: "r1",
    });
    expect(openPane(base, { kind: "settings", of: "c1", page: null })).toHaveLength(4);
  });

  it("each conversation keeps its own panes; task, agent and person are one each", () => {
    const two = openPane(openPane([], { kind: "agent", id: "a" }), { kind: "agent", id: "b" });
    expect(two).toEqual([{ kind: "agent", id: "b" }]);
  });

  it("closes one pane", () => {
    expect(closePane(base, "agent")).toEqual([base[0], base[2]]);
  });

  it("toggles", () => {
    expect(togglePane(base, { kind: "threads", of: "c1", thread: null })).toEqual(base.slice(1));
    expect(togglePane([], { kind: "files", of: "c1", attachment: null })).toHaveLength(1);
  });

  it("moves a pane to a new position", () => {
    expect(movePane(base, "files:c1", 0).map((p) => p.kind)).toEqual(["files", "threads", "agent"]);
    expect(movePane(base, "threads:c1", 9).map((p) => p.kind)).toEqual([
      "agent",
      "files",
      "threads",
    ]);
  });

  it("a new route drops the old route's panes", () => {
    expect(settlePanes(base, "c2")).toEqual([{ kind: "agent", id: "a" }]);
  });
});

describe("onScreen", () => {
  const panes: Panes = [
    { kind: "agent", id: "a" },
    { kind: "threads", of: "c1", thread: "r1" },
  ];

  it("everything open is visible at regular width", () => {
    expect(onScreen("c1", panes, false)).toEqual({
      conversations: ["c1"],
      threads: [{ conversationId: "c1", rootId: "r1" }],
    });
  });

  it("compact shows the last pane only, or the route's conversation", () => {
    expect(onScreen("c1", panes, true)).toEqual({
      conversations: [],
      threads: [{ conversationId: "c1", rootId: "r1" }],
    });
    expect(onScreen("c1", [], true).conversations).toEqual(["c1"]);
  });
});
