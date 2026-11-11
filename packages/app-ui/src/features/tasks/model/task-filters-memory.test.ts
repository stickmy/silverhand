import { describe, expect, it } from "vitest";
import { parseTaskFilters } from "./task-filters-memory";
import { DEFAULT_TASK_FILTERS, type TaskFilters } from "./task-filters";

describe("parseTaskFilters", () => {
  it("round-trips a full selection", () => {
    const filters: TaskFilters = {
      container: "direct_messages",
      workspaceId: null,
      channelId: "dp-1",
      assignee: { kind: "person", person: { type: "agent", id: "a-1" } },
      creator: { kind: "person", person: { type: "user", id: "u-1" } },
    };
    expect(parseTaskFilters(JSON.stringify(filters))).toEqual(filters);
  });

  it("falls back to the defaults for missing or unreadable storage", () => {
    expect(parseTaskFilters(null)).toEqual(DEFAULT_TASK_FILTERS);
    expect(parseTaskFilters("{not json")).toEqual(DEFAULT_TASK_FILTERS);
    expect(parseTaskFilters("42")).toEqual(DEFAULT_TASK_FILTERS);
  });

  it("drops malformed fields one by one", () => {
    const raw = JSON.stringify({
      container: "galaxy",
      workspaceId: "ws-1",
      channelId: 7,
      assignee: { kind: "person", person: { type: "robot", id: "x" } },
      creator: { kind: "unassigned" },
    });
    expect(parseTaskFilters(raw)).toEqual(DEFAULT_TASK_FILTERS);
  });

  it("keeps No assignee on the assignee axis only", () => {
    const raw = JSON.stringify({ assignee: { kind: "unassigned" } });
    expect(parseTaskFilters(raw).assignee).toEqual({ kind: "unassigned" });
  });

  it("does not keep a workspace without its container", () => {
    const raw = JSON.stringify({ container: null, workspaceId: "ws-1" });
    expect(parseTaskFilters(raw).workspaceId).toBeNull();
  });
});
