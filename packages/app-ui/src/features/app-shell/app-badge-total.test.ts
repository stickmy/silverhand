import { describe, expect, it } from "vitest";
import type { DirectPairListItem } from "@silverhand/shared";
import { appBadgeTotal } from "./app-badge-total";

function pair(id: string, unread: number, deleted = false): DirectPairListItem {
  return {
    conversation_id: id,
    counterpart: deleted
      ? { status: "deleted", kind: "agent", id: `a-${id}` }
      : {
          status: "active",
          kind: "agent",
          id: `a-${id}`,
          name: id,
          display_name: id,
          avatar_url: null,
          runtime: "claude",
        },
    unread_count: unread,
    read_only: false,
    created_at: 1,
  };
}

describe("appBadgeTotal", () => {
  it("adds direct-message unread to the workspace roll-up", () => {
    expect(appBadgeTotal(3, [pair("a", 2), pair("b", 0)])).toBe(5);
  });

  it("counts direct messages before the roll-up has loaded", () => {
    expect(appBadgeTotal(undefined, [pair("a", 1)])).toBe(1);
  });

  it("leaves out pairs the sidebar hides", () => {
    expect(appBadgeTotal(0, [pair("gone", 4, true)])).toBe(0);
  });

  it("stays unknown while nothing has loaded", () => {
    expect(appBadgeTotal(undefined, [])).toBeUndefined();
  });
});
