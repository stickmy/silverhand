import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import type { DirectPairListItem } from "@silverhand/shared";
import { decideDraftHandoff, type DraftHandoffFacts } from "./hooks";
import {
  mergeDirectLineRows,
  directLinePairVisibleInSidebar,
  type DirectLineDraft,
} from "./direct-line-rows";

const AGENT = "a-target";

function pair(conversationId: string, agentId: string): DirectPairListItem {
  return {
    conversation_id: conversationId,
    counterpart: {
      status: "active",
      kind: "agent",
      id: agentId,
      name: agentId,
      display_name: `Agent ${agentId}`,
      avatar_url: "blob-from-pair",
      runtime: "claude",
    },
    unread_count: 3,
    read_only: false,
    created_at: 1,
  };
}

/** The draft's own counterpart — read from the agent row, deliberately different
 *  from the pair's so a merged row can be shown to carry the PAIR's. */
const draft: DirectLineDraft = {
  target: { kind: "agent", id: AGENT },
  counterpart: {
    status: "active",
    kind: "agent",
    id: AGENT,
    name: AGENT,
    display_name: `Agent ${AGENT}`,
    avatar_url: "blob-from-agent-read",
    runtime: "claude",
  },
};

describe("mergeDirectLineRows — one line per agent, and who owns it", () => {
  it("is the pair list, in the server order, when no draft is open", () => {
    const rows = mergeDirectLineRows({
      pairs: [pair("c-1", "a-1"), pair("c-2", "a-2")],
      draft: null,
      selectedConversationId: "c-2",
    });
    expect(rows.map((r) => [r.kind, r.key, r.selected])).toEqual([
      ["pair", "c-1", false],
      ["pair", "c-2", true],
    ]);
  });

  it("appends a draft with no pair, where a line just opened belongs", () => {
    const rows = mergeDirectLineRows({
      pairs: [pair("c-1", "a-1")],
      draft,
      selectedConversationId: null,
    });
    expect(rows).toHaveLength(2);
    expect(rows[1]).toEqual({
      kind: "draft",
      key: `draft:agent:${AGENT}`,
      target: { kind: "agent", id: AGENT },
      counterpart: draft.counterpart,
      unread: 0,
      selected: true,
    });
  });

  it("THE REGRESSION: a pair for the draft's agent yields ONE row, and the DRAFT owns it", () => {
    // The repro the review filed: A's pair exists (A wrote first, or the first
    // send minted it and then failed) while `/chat/d/new/A` is still the open
    // pane. The old code drew only the pair's row, unselected.
    const rows = mergeDirectLineRows({
      pairs: [pair("c-pair", AGENT)],
      draft,
      // The pair is not the open conversation — the DRAFT route is. This is
      // exactly the state in which the row used to go unselected.
      selectedConversationId: null,
    });
    // One row, it is the draft's, and it is selected — so the sidebar agrees with
    // the pane about what is open, and `kind: 'draft'` is what makes a click stay
    // on the draft instead of navigating to the pair.
    expect(rows).toMatchObject([
      { kind: "draft", target: { kind: "agent", id: AGENT }, selected: true },
    ]);
  });

  it("keeps the pair's position, identity and unread on the merged row", () => {
    const rows = mergeDirectLineRows({
      pairs: [pair("c-1", "a-1"), pair("c-pair", AGENT), pair("c-3", "a-3")],
      draft,
      selectedConversationId: null,
    });
    // Same line, same place: a draft that jumped to the tail the moment the pair
    // landed would read as a row appearing and another disappearing. Keyed by the
    // PAIR, so convergence swaps the row's behaviour without remounting it (and
    // never collides with the tail key). And the pair is the authority on the
    // counterpart and the count — the agent may have written first, which is one
    // way the pair got there.
    expect(rows).toMatchObject([
      { kind: "pair", key: "c-1" },
      {
        kind: "draft",
        key: "c-pair",
        counterpart: pair("c-pair", AGENT).counterpart,
        unread: 3,
      },
      { kind: "pair", key: "c-3" },
    ]);
  });

  it("never matches a HUMAN counterpart sharing the id, and never merges twice", () => {
    // Ids are per-kind. A human row with the same id is a different line, so the
    // draft must still get its own.
    const human: DirectPairListItem = {
      ...pair("c-human", AGENT),
      counterpart: {
        status: "active",
        kind: "human",
        id: AGENT,
        name: "shiki",
        display_name: "shiki",
        avatar_url: null,
        runtime: null,
      },
    };
    const rows = mergeDirectLineRows({
      pairs: [human],
      draft,
      selectedConversationId: null,
    });
    expect(rows.map((r) => r.kind)).toEqual(["pair", "draft"]);
    // And at most one row can be the draft's, however many pairs are listed.
    expect(rows.filter((r) => r.kind === "draft")).toHaveLength(1);
  });

  it("highlights EXACTLY one row in each reachable configuration", () => {
    // The two selection carriers are mutually exclusive by route:
    // `/chat/d/new/:agentId` puts nothing in `selectedChannelId` (app.tsx: «Nothing
    // is selected while it is set»), so "a draft is open" and "a conversation is
    // open" never arrive together. Both cells must land on one highlight — a
    // sidebar with none is the reported bug, and one with two would be the same
    // disagreement in the other direction.
    const pairs = [pair("c-pair", AGENT), pair("c-2", "a-2")];
    expect(
      mergeDirectLineRows({ pairs, draft, selectedConversationId: null })
        .filter((r) => r.selected)
        .map((r) => r.kind),
    ).toEqual(["draft"]);
    expect(
      mergeDirectLineRows({ pairs, draft: null, selectedConversationId: "c-2" })
        .filter((r) => r.selected)
        .map((r) => r.kind),
    ).toEqual(["pair"]);
  });

  it("a pair row is selected only by the open conversation", () => {
    const rows = mergeDirectLineRows({
      pairs: [pair("c-1", "a-1")],
      draft: null,
      selectedConversationId: "c-1",
    });
    expect(rows).toMatchObject([{ selected: true }]);
    expect(
      mergeDirectLineRows({
        pairs: [pair("c-1", "a-1")],
        draft: null,
        selectedConversationId: null,
      }),
    ).toMatchObject([{ selected: false }]);
  });

  it("renders nothing to merge when the viewer has neither", () => {
    expect(mergeDirectLineRows({ pairs: [], draft: null, selectedConversationId: null })).toEqual(
      [],
    );
  });
});

function deletedPair(
  conversationId: string,
  kind: "agent" | "human",
  id: string,
): DirectPairListItem {
  return {
    conversation_id: conversationId,
    // The deleted arm carries only kind + id — the principal is gone, so there is
    // no name/avatar left to read.
    counterpart: { status: "deleted", kind, id },
    unread_count: 0,
    read_only: false,
    created_at: 1,
  };
}

describe("directLinePairVisibleInSidebar — a gone principal leaves the sidebar (#task:2094)", () => {
  // @shiki: a deleted account's pair should not sit in DIRECT MESSAGES by default.
  // SIDEBAR-ONLY — saved/search read pairs by other paths and still surface the
  // line (with its "(deleted account)" label), so the rule lives here, at the
  // sidebar's sole row builder, not in the pair query.
  it("hides a deleted counterpart, agent and human alike", () => {
    expect(directLinePairVisibleInSidebar(deletedPair("c-x", "agent", "a-gone"))).toBe(false);
    expect(directLinePairVisibleInSidebar(deletedPair("c-y", "human", "h-gone"))).toBe(false);
  });

  it("shows every active counterpart", () => {
    expect(directLinePairVisibleInSidebar(pair("c-1", "a-1"))).toBe(true);
    const human: DirectPairListItem = {
      ...pair("c-h", "a-1"),
      counterpart: {
        status: "active",
        kind: "human",
        id: "h-1",
        name: "shiki",
        display_name: "shiki",
        avatar_url: null,
        runtime: null,
      },
    };
    expect(directLinePairVisibleInSidebar(human)).toBe(true);
  });

  it("mergeDirectLineRows drops deleted pairs, keeping the active ones in server order", () => {
    const rows = mergeDirectLineRows({
      pairs: [
        pair("c-1", "a-1"),
        deletedPair("c-gone-agent", "agent", "a-gone"),
        pair("c-2", "a-2"),
        deletedPair("c-gone-human", "human", "h-gone"),
      ],
      draft: null,
      selectedConversationId: null,
    });
    expect(rows.map((r) => r.key)).toEqual(["c-1", "c-2"]);
  });

  it("a deleted pair for the draft's agent still yields the draft row, never the deleted pair", () => {
    // The pair is filtered before the draft-merge sees it, so a deleted pair can
    // never become the draft's carrier row; the draft is appended on its own key.
    const rows = mergeDirectLineRows({
      pairs: [deletedPair("c-gone", "agent", AGENT)],
      draft,
      selectedConversationId: null,
    });
    expect(rows).toMatchObject([{ kind: "draft", key: `draft:agent:${AGENT}`, selected: true }]);
  });
});

describe("composed with decideDraftHandoff — the three cells the review named", () => {
  // The composition IS the argument. The sidebar does not restate "dirty",
  // "pending" or "failed"; it is handed a draft exactly while the pane's rule says
  // `stay`, so pinning the pair of functions pins all three cells with ONE
  // decider. A second copy of these conditions in the sidebar is the defect that
  // was just fixed.
  const facts = (over: Partial<DraftHandoffFacts>): DraftHandoffFacts => ({
    existingPairId: "c-pair",
    draftDirty: false,
    sendPending: false,
    sendFailed: false,
    ...over,
  });

  /** What the sidebar sees for a route whose pane applied `decideDraftHandoff`. */
  function rowsAfterHandoff(over: Partial<DraftHandoffFacts>) {
    const decision = decideDraftHandoff(facts(over));
    const pairs = [pair("c-pair", AGENT)];
    return decision.kind === "stay"
      ? mergeDirectLineRows({ pairs, draft, selectedConversationId: null })
      : // Handoff: the draft route is gone, and the pane is now the pair's.
        mergeDirectLineRows({
          pairs,
          draft: null,
          selectedConversationId: decision.conversationId,
        });
  }

  for (const [cell, over] of [
    ["DIRTY — unsent text", { draftDirty: true }],
    ["PENDING — a send in flight", { sendPending: true }],
    ["POST-MINT ERROR — the pair exists and the send failed", { sendFailed: true }],
  ] as const) {
    it(`${cell}: one row, selected, and opening it stays on the draft`, () => {
      const rows = rowsAfterHandoff(over);
      expect(rows).toHaveLength(1);
      expect(rows[0]).toMatchObject({
        kind: "draft",
        target: { kind: "agent", id: AGENT },
        selected: true,
      });
    });
  }

  it("EMPTY draft: handoff succeeds and the line becomes an ordinary pair row", () => {
    // The other half of the property — the draft row is not sticky. When the pane
    // releases the route, the same line is a plain pair row, selected because the
    // pair is now the open conversation.
    const rows = rowsAfterHandoff({});
    expect(rows).toHaveLength(1);
    expect(rows[0]).toMatchObject({
      kind: "pair",
      key: "c-pair",
      selected: true,
    });
  });
});

/** Source with comments removed, so a probe that comments a call out cannot pass.
 *  (Only `//` that does not follow a `:` is stripped, so a `scheme://` survives.) */
function code(source: string): string {
  return source.replace(/\/\*[\s\S]*?\*\//g, "").replace(/(^|[^:])\/\/.*$/gm, "$1");
}
function read(relative: string): string {
  return code(readFileSync(fileURLToPath(new URL(relative, import.meta.url)), "utf8"));
}

describe("the sidebar and the shell defer to the seam (the wiring half)", () => {
  const panelSource = read("../../pages/chat/sidebar/direct-messages-panel.tsx");
  const appSource = read("../../pages/chat/chat-browse-sidebar.tsx");

  it("the section merges through the seam and decides nothing itself", () => {
    expect(panelSource).toContain("mergeDirectLineRows({");
    // The exact shape of the defect: suppressing the draft row on the pair's
    // existence. The section must not consult the pair list for that at all.
    expect(panelSource).not.toContain("findAgentDirectPair");
  });

  it("opening the draft row re-opens THE DRAFT, never the pair", () => {
    // `handleMessageAgent` prefers an existing pair — correct for the agent
    // page's `Message` entry, and a data-loss path for this row.
    expect(appSource).toContain("useEventCallback(nav.toDirectDraft)");
    expect(appSource).toContain("onSelectDirectDraft={selectDirectDraft}");
    expect(appSource).not.toContain("onSelectDirectDraft={handleMessageAgent}");
  });
});

describe("human draft convergence", () => {
  it("matches both kind and id, so an agent sharing the id is a different row", () => {
    const human: DirectPairListItem = {
      ...pair("human-pair", AGENT),
      counterpart: {
        status: "active",
        kind: "human",
        id: AGENT,
        name: "human",
        display_name: "Human",
        avatar_url: null,
        runtime: null,
      },
    };
    const humanDraft: DirectLineDraft = {
      target: { kind: "human", id: AGENT },
      counterpart: human.counterpart,
    };
    const rows = mergeDirectLineRows({
      pairs: [pair("agent-pair", AGENT), human],
      draft: humanDraft,
      selectedConversationId: null,
    });
    expect(rows.map((row) => [row.kind, row.key])).toEqual([
      ["pair", "agent-pair"],
      ["draft", "human-pair"],
    ]);
    const unminted = mergeDirectLineRows({
      pairs: [pair("agent-pair", AGENT)],
      draft: humanDraft,
      selectedConversationId: null,
    });
    expect(unminted[1]?.key).toBe(`draft:human:${AGENT}`);
  });
});
