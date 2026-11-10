import { describe, expect, it } from "vitest";
import type { ChannelAttentionModeRowDto } from "@silverhand/shared";
import { collapsedRevealRows } from "@/features/workspaces/home-grouping";
import { directRowBadge, favoriteRowBadge } from "./row-badges";

function attention(mode: "all" | "mentions" | "mute", directed = 0): ChannelAttentionModeRowDto {
  return {
    conversation_id: "channel",
    mode,
    effective_mode: mode,
    directed_unread_count: directed,
  };
}

describe("collapsed fixed sidebar groups", () => {
  it("reveals unread DMs alongside the selected draft, once each in list order; read rows disappear", () => {
    const rows = [
      { id: "unread", unread: 2, selected: false },
      { id: "read", unread: 0, selected: false },
      { id: "draft", unread: 0, selected: true },
      { id: "selected-unread", unread: 1, selected: true },
    ];
    const reveal = () =>
      collapsedRevealRows(rows, {
        pokeUnread: true,
        isSelected: (r) => r.selected,
        isLoud: (r) => directRowBadge(r.unread)?.tier === "loud",
      }).map((r) => r.id);
    expect(reveal()).toEqual(["unread", "draft", "selected-unread"]);
    rows[0]!.unread = 0;
    expect(reveal()).toEqual(["draft", "selected-unread"]);
  });

  it("uses the same personal attention verdict for Starred reveal and painting", () => {
    const item = { roster_policy: "open" as const, unread_count: 5 };
    expect(favoriteRowBadge(item, attention("all"))?.tier).toBe("loud");
    expect(favoriteRowBadge(item, attention("mentions"))?.tier).toBe("quiet");
    expect(favoriteRowBadge(item, attention("mute"))).toBeNull();
    expect(favoriteRowBadge(item, undefined)).toBeNull();
    for (const mode of ["mentions", "mute"] as const) {
      expect(favoriteRowBadge(item, attention(mode, 1))).toEqual({ count: 1, tier: "loud" });
    }
    expect(favoriteRowBadge({ roster_policy: "fixed", unread_count: 2 }, undefined)?.tier).toBe(
      "loud",
    );
  });

  it("keeps selected quiet favorites, but does not reveal unselected quiet or unknown rows", () => {
    const rows = [
      { id: "quiet", selected: false, attention: attention("mentions") },
      { id: "selected", selected: true, attention: undefined },
      { id: "mention", selected: false, attention: attention("mute", 1) },
      { id: "unknown", selected: false, attention: undefined },
    ];
    expect(
      collapsedRevealRows(rows, {
        pokeUnread: true,
        isSelected: (r) => r.selected,
        isLoud: (r) =>
          favoriteRowBadge({ roster_policy: "open", unread_count: 3 }, r.attention)?.tier ===
          "loud",
      }).map((r) => r.id),
    ).toEqual(["selected", "mention"]);
  });
});
