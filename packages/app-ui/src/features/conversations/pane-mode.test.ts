import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { ConversationPlacementKindSchema } from "@silverhand/shared";
import type { ConversationPlacementKind } from "@silverhand/shared";
import type { ConversationPaneDescriptor } from "./pane-descriptor";
import {
  paneCapabilities,
  placementOfPaneMode,
  projectConversationPaneMode,
  rosterQueryTarget,
  inheritedPaneCapabilities,
  composerDirectory,
  inheritedComposerDirectory,
  DEFAULT_PANE_CAPABILITIES,
  type ConversationPaneMode,
  type ComposerWorkspaceCoordinate,
} from "./pane-mode";

// #task:2039 — the pane's placement is read ONCE, here. Four dogfood findings
// came from four components each guessing it (`workspaceId === null`, the route,
// a local boolean), so the evidence this file owes is not "the DM header shows a
// face" (that is UI, untested by rule) but that the projection is:
//   1. TOTAL over the stored placement union — a placement with no pane mode must
//      land in `unsupported`, never fall through into channel chrome;
//   2. BIDIRECTIONAL — mode → placement inverts the projection, so a rule keyed
//      by placement and a rule keyed by mode (capabilities) cannot drift into two
//      different answers for the same conversation;
//   3. EXHAUSTIVE per capability — every capability is asserted in both
//      directions: what a mode grants, and which modes grant it. A new mode that
//      quietly inherits `hasRoster` fails the second half.
//   4. A READ GATE, not paint (@Cindy's review): the capability has to turn the
//      roster QUERIES off at the source. A surface that fires them and then
//      renders nothing because a pair happens to have no roster row has moved its
//      capability into the data, where the next placement with a non-empty roster
//      silently inherits channel behaviour.

function descriptor(over: Partial<ConversationPaneDescriptor> = {}): ConversationPaneDescriptor {
  return {
    id: "c-1",
    title: "general",
    placement_kind: "workspace",
    roster_policy: "open",
    closed_at: null,
    archived_at: null,
    workspace_id: "w-1",
    ...over,
  };
}

const PLACEMENTS: readonly ConversationPlacementKind[] = ConversationPlacementKindSchema.options;

const MODE_SAMPLES: Record<ConversationPaneMode["kind"], ConversationPaneMode> = {
  workspace_channel: { kind: "workspace_channel", workspaceId: "w-1" },
  direct_message: { kind: "direct_message", counterpart: null },
  // Not a stored placement after #task:2072 c4 — the alphabet is
  // `workspace | direct_pair`. This is the old-client-new-server case: a value a
  // newer server can send that this build's compiled union does not contain, which
  // the projection funnels to `unsupported`. The cast lives in the test only; the
  // production type stays narrow.
  unsupported: {
    kind: "unsupported",
    placement: "future_placement" as ConversationPlacementKind,
  },
};
const MODE_KINDS = Object.keys(MODE_SAMPLES) as ConversationPaneMode["kind"][];

describe("projectConversationPaneMode — total over the placement union", () => {
  it("covers every stored placement kind", () => {
    // Not a loop for its own sake: the union lives in the DB schema, and a fifth
    // arm added there must show up here as a decision instead of as whatever the
    // `default` happens to do.
    for (const placement of PLACEMENTS) {
      const mode = projectConversationPaneMode(descriptor({ placement_kind: placement }));
      expect(MODE_KINDS).toContain(mode.kind);
    }
  });

  it("maps the two known shapes and nothing else", () => {
    expect(
      PLACEMENTS.map((p) => [
        p,
        projectConversationPaneMode(descriptor({ placement_kind: p })).kind,
      ]),
    ).toEqual([
      ["workspace", "workspace_channel"],
      ["direct_pair", "direct_message"],
    ]);
  });

  it("funnels an unknown wire placement to `unsupported` (old client, new server)", () => {
    // The default arm is not dead after c4: this renderer reads placement off an
    // unvalidated DTO and can be older than the server it talks to. A value
    // outside the compiled alphabet must fail safe to `unsupported`, never inherit
    // channel chrome by falling through. The cast lives here only.
    expect(
      projectConversationPaneMode(
        descriptor({ placement_kind: "future_placement" as ConversationPlacementKind }),
      ),
    ).toEqual({ kind: "unsupported", placement: "future_placement" });
  });

  it("round-trips through the placement inverse", () => {
    // The bidirectional half: a mode names exactly the placement it came from,
    // which is what lets a placement-keyed rule and a mode-keyed rule agree.
    for (const placement of PLACEMENTS) {
      expect(
        placementOfPaneMode(projectConversationPaneMode(descriptor({ placement_kind: placement }))),
      ).toBe(placement);
    }
    for (const kind of MODE_KINDS) {
      const mode = MODE_SAMPLES[kind];
      expect(
        projectConversationPaneMode(descriptor({ placement_kind: placementOfPaneMode(mode) })).kind,
      ).toBe(kind);
    }
  });

  it("carries the workspace coordinate it was handed, including a null one", () => {
    expect(projectConversationPaneMode(descriptor())).toEqual({
      kind: "workspace_channel",
      workspaceId: "w-1",
    });
    expect(projectConversationPaneMode(descriptor({ workspace_id: null }))).toEqual({
      kind: "workspace_channel",
      workspaceId: null,
    });
  });

  it("carries the counterpart a pair resolved, and null when none was", () => {
    const counterpart = {
      status: "active",
      kind: "agent",
      id: "a-1",
      name: "norman",
      display_name: "Norman",
      avatar_url: null,
      runtime: "claude",
    } as const;
    expect(
      projectConversationPaneMode(
        descriptor({ placement_kind: "direct_pair", workspace_id: null, counterpart }),
      ),
    ).toEqual({ kind: "direct_message", counterpart });
    expect(
      projectConversationPaneMode(
        descriptor({ placement_kind: "direct_pair", workspace_id: null }),
      ),
    ).toEqual({ kind: "direct_message", counterpart: null });
  });
});

describe("paneCapabilities — every affordance, both directions", () => {
  it("states the whole capability set per mode", () => {
    expect(paneCapabilities(MODE_SAMPLES.workspace_channel)).toEqual({
      headerIdentity: "channel_hash",
      hasRoster: true,
      workspaceScope: "w-1",
    });
    expect(paneCapabilities(MODE_SAMPLES.direct_message)).toEqual({
      headerIdentity: "counterpart",
      hasRoster: false,
      workspaceScope: null,
    });
    expect(paneCapabilities(MODE_SAMPLES.unsupported)).toEqual({
      headerIdentity: "title_only",
      hasRoster: false,
      workspaceScope: null,
    });
  });

  it("grants roster surfaces to EXACTLY one mode", () => {
    // The inverse assertion, and the one that catches the failure this seam
    // exists for: a new mode inheriting the member strip / members button /
    // add-member picker by default.
    expect(MODE_KINDS.filter((k) => paneCapabilities(MODE_SAMPLES[k]).hasRoster)).toEqual([
      "workspace_channel",
    ]);
  });

  it("gives each mode its own header identity — no two share one", () => {
    const identities = MODE_KINDS.map((k) => paneCapabilities(MODE_SAMPLES[k]).headerIdentity);
    expect(new Set(identities).size).toBe(MODE_KINDS.length);
    expect(identities).toEqual(["channel_hash", "counterpart", "title_only"]);
  });

  it("scopes workspace-only affordances to the workspace mode", () => {
    expect(
      MODE_KINDS.filter((k) => paneCapabilities(MODE_SAMPLES[k]).workspaceScope !== null),
    ).toEqual(["workspace_channel"]);
    // And a workspace mode with no coordinate withholds them too — the pane
    // paints what it was handed instead of asserting one.
    expect(
      paneCapabilities({ kind: "workspace_channel", workspaceId: null }).workspaceScope,
    ).toBeNull();
  });
});

describe("rosterQueryTarget — the capability turns the READS off, not just the paint", () => {
  const target = (placement: ConversationPlacementKind, allowMemberEffects: boolean) =>
    rosterQueryTarget(
      paneCapabilities(projectConversationPaneMode(descriptor({ placement_kind: placement }))),
      "c-1",
      allowMemberEffects,
    );

  it("answers for every placement × membership cell", () => {
    // Two independent facts disable the read and they COMPOSE here rather than at
    // each call site: the placement having no roster, and the viewer not being a
    // member of one that does (a non-member browsing a public channel).
    expect(
      PLACEMENTS.flatMap((p) => [
        [p, true, target(p, true)],
        [p, false, target(p, false)],
      ]),
    ).toEqual([
      ["workspace", true, "c-1"],
      ["workspace", false, null],
      ["direct_pair", true, null],
      ["direct_pair", false, null],
    ]);
  });

  it("yields a conversation to read for EXACTLY the placement that has a roster", () => {
    expect(PLACEMENTS.filter((p) => target(p, true) !== null)).toEqual(["workspace"]);
  });

  it("returns the id it was handed — the gate never invents a target", () => {
    expect(rosterQueryTarget(DEFAULT_PANE_CAPABILITIES, "c-other", true)).toBe("c-other");
  });
});

describe("DEFAULT_PANE_CAPABILITIES — a surface with no pane is unchanged", () => {
  it("is the workspace-channel answer, with no assumed coordinate", () => {
    // The context default. It has to be what every surface did BEFORE the
    // projection existed, so introducing the provider changed no behaviour; the
    // workspace is null because a coordinate must be carried, never assumed.
    expect(DEFAULT_PANE_CAPABILITIES).toEqual({
      headerIdentity: "channel_hash",
      hasRoster: true,
      workspaceScope: null,
    });
  });
});

describe("inheritedPaneCapabilities — a thread is its parent (#task:2098)", () => {
  // The second finding of the same class as #task:2039: a pane that is NOT a
  // conversation (a thread shares its parent's) had no projection at all, so it
  // read the channel-shaped context default and answered `Not here` in a DM for an
  // agent the mainline showed as Online. Both arms are pinned, because the failure
  // mode of the fix is erasing the channel's REAL `Not here` along with the DM's
  // false one.
  const parent = (placement: ConversationPlacementKind) =>
    descriptor({
      placement_kind: placement,
      workspace_id: placement === "workspace" ? "w-1" : null,
    });

  it("gives a DM thread the pair capabilities — no roster, so no `Not here`", () => {
    expect(inheritedPaneCapabilities(parent("direct_pair"))).toEqual({
      headerIdentity: "counterpart",
      hasRoster: false,
      workspaceScope: null,
    });
  });

  it("leaves a channel thread the roster, so a real non-member still reads `Not here`", () => {
    expect(inheritedPaneCapabilities(parent("workspace"))).toEqual({
      headerIdentity: "channel_hash",
      hasRoster: true,
      workspaceScope: "w-1",
    });
  });

  it("is the parent projection itself for every placement — never a second mapping", () => {
    // The anti-drift assertion, and the reason acceptance #3 forbids re-deriving
    // placement in the thread pane: inheritance is defined as the parent's own
    // answer, so mainline and thread cannot disagree about one conversation.
    for (const placement of PLACEMENTS) {
      expect(inheritedPaneCapabilities(parent(placement))).toEqual(
        paneCapabilities(projectConversationPaneMode(parent(placement))),
      );
    }
  });

  it("withholds when the parent has not resolved — not the channel default", () => {
    // A cold deep link to `?thread=<root>`: the thread stream has its own query and
    // can settle before the parent conversation is known. Inheriting the context
    // default here would reproduce the exact bug, so an unknown parent withholds —
    // the surface shows no presence line, which is already what it does while a
    // roster read is unsettled, rather than asserting a membership it cannot know.
    expect(inheritedPaneCapabilities(null)).toEqual({
      headerIdentity: "title_only",
      hasRoster: false,
      workspaceScope: null,
    });
    expect(inheritedPaneCapabilities(null)).not.toEqual(DEFAULT_PANE_CAPABILITIES);
  });

  it("turns the thread pane roster READ off for a pair and on for a channel", () => {
    // Same composition the pane uses. `hasRoster` gates the query, so the DM thread
    // never starts the read whose empty response was being painted as `Not here`.
    expect(
      PLACEMENTS.map((p) => [
        p,
        rosterQueryTarget(inheritedPaneCapabilities(parent(p)), "c-1", true),
      ]),
    ).toEqual([
      ["workspace", "c-1"],
      ["direct_pair", null],
    ]);
    expect(rosterQueryTarget(inheritedPaneCapabilities(null), "c-1", true)).toBeNull();
  });
});

describe("composerDirectory — the three-arm authority, exhaustive over the mode union", () => {
  // The trap this projection closes is a two-value one: `paneCapabilities`
  // answers `hasRoster:false` for a DirectPair AND for unsupported/unresolved, so
  // `hasRoster ? workspace : viewer` folds three states into two and hands a
  // cold-start pane the viewer's whole owned-agent directory. Every mode kind must
  // land in a NAMED arm, and the coordinate must reach the workspace arm and
  // nowhere else.
  const COORD: ComposerWorkspaceCoordinate = {
    workspaceName: "Acme",
    hasChannelNamespace: true,
  };

  it("projects every mode kind into a named arm — nothing folds", () => {
    // The `hasRoster:false` collision is the whole reason for three arms: the two
    // modes that share it must NOT share a directory arm.
    expect(MODE_KINDS.map((k) => composerDirectory(MODE_SAMPLES[k], COORD).kind)).toEqual([
      "workspace",
      "viewer",
      "withheld",
    ]);
  });

  it("gives the workspace arm the coordinate it was handed, withholds an unresolved workspace", () => {
    expect(composerDirectory(MODE_SAMPLES.workspace_channel, COORD)).toEqual({
      kind: "workspace",
      workspaceId: "w-1",
      workspaceName: "Acme",
      hasChannelNamespace: true,
    });
    // A workspace row that has not resolved has no coordinate yet, so it is
    // unresolved — not resolved-empty. It withholds rather than settling the
    // workspace arm with a null id (which would leak a source-known-empty read).
    expect(
      composerDirectory(
        { kind: "workspace_channel", workspaceId: null },
        {
          workspaceName: null,
          hasChannelNamespace: false,
        },
      ),
    ).toEqual({ kind: "withheld" });
  });

  it("consults the coordinate ONLY in the workspace arm", () => {
    // The viewer and withheld arms carry no workspace fields — a DirectPair reads
    // the viewer's own directory, not a workspace's, so a stray coordinate must not
    // change either answer.
    const other: ComposerWorkspaceCoordinate = {
      workspaceName: "Other",
      hasChannelNamespace: false,
    };
    expect(composerDirectory(MODE_SAMPLES.direct_message, COORD)).toEqual({
      kind: "viewer",
      counterpartAgent: null,
    });
    expect(composerDirectory(MODE_SAMPLES.direct_message, other)).toEqual({
      kind: "viewer",
      counterpartAgent: null,
    });
    expect(composerDirectory(MODE_SAMPLES.unsupported, COORD)).toEqual({ kind: "withheld" });
    expect(composerDirectory(MODE_SAMPLES.unsupported, other)).toEqual({ kind: "withheld" });
  });

  it("carries an active agent counterpart into the viewer arm; null for human / deleted", () => {
    // The counterpart's mentionable identity rides the arm even when the viewer
    // does not own it. A human counterpart and a deleted agent (kind+id only, no
    // name/runtime) both project to null.
    const agentCounterpart = {
      kind: "direct_message",
      counterpart: {
        status: "active",
        kind: "agent",
        id: "a-9",
        name: "norman",
        display_name: "Norman",
        avatar_url: null,
        runtime: "claude",
      },
    } as const;
    expect(composerDirectory(agentCounterpart, COORD)).toEqual({
      kind: "viewer",
      counterpartAgent: {
        id: "a-9",
        name: "norman",
        display_name: "Norman",
        avatar_url: null,
        runtime: "claude",
      },
    });
    const humanCounterpart = {
      kind: "direct_message",
      counterpart: {
        status: "active",
        kind: "human",
        id: "u-3",
        name: "shiki",
        display_name: "shiki",
        avatar_url: null,
        runtime: null,
      },
    } as const;
    expect(composerDirectory(humanCounterpart, COORD)).toEqual({
      kind: "viewer",
      counterpartAgent: null,
    });
    // A deleted agent counterpart projects to null, not an id-only row.
    const deletedAgentCounterpart = {
      kind: "direct_message",
      counterpart: { status: "deleted", kind: "agent", id: "a-gone" },
    } as const;
    expect(composerDirectory(deletedAgentCounterpart, COORD)).toEqual({
      kind: "viewer",
      counterpartAgent: null,
    });
  });

  it("maps a DirectPair to the viewer arm and an unknown placement to withheld", () => {
    // The bidirectional half restated for the directory: the DirectPair is the ONE
    // mode that reads the viewer's own directory, and withheld is where an
    // unresolved/unsupported placement fails safe (empty, source-unknown), never
    // the viewer directory by default.
    expect(
      MODE_KINDS.filter((k) => composerDirectory(MODE_SAMPLES[k], COORD).kind === "viewer"),
    ).toEqual(["direct_message"]);
    expect(
      MODE_KINDS.filter((k) => composerDirectory(MODE_SAMPLES[k], COORD).kind === "withheld"),
    ).toEqual(["unsupported"]);
  });
});

describe("inheritedComposerDirectory — a thread is its parent (#task:2098)", () => {
  const COORD: ComposerWorkspaceCoordinate = {
    workspaceName: "Acme",
    hasChannelNamespace: true,
  };
  const parent = (placement: ConversationPlacementKind) =>
    descriptor({
      placement_kind: placement,
      workspace_id: placement === "workspace" ? "w-1" : null,
    });

  it("is the parent projection itself for every placement — never a second mapping", () => {
    // The anti-drift assertion, same as capabilities: inheritance IS the parent's
    // own directory, so the thread composer and the mainline composer cannot
    // disagree about which directory a conversation offers.
    for (const placement of PLACEMENTS) {
      expect(inheritedComposerDirectory(parent(placement), COORD)).toEqual(
        composerDirectory(projectConversationPaneMode(parent(placement)), COORD),
      );
    }
  });

  it("withholds when the parent has not resolved — not the viewer directory", () => {
    // A cold deep link to `?thread=<root>` before the parent conversation is known.
    // Inheriting anything but withheld here would leak a directory (the viewer's
    // owned agents, or a channel's members) for a placement not yet determined.
    expect(inheritedComposerDirectory(null, COORD)).toEqual({ kind: "withheld" });
  });
});

/** Source with comments removed, so a probe that comments a gate out cannot pass.
 *  (Only `//` that does not follow a `:` is stripped, so a `scheme://` survives.) */
function code(source: string): string {
  return source.replace(/\/\*[\s\S]*?\*\//g, "").replace(/(^|[^:])\/\/.*$/gm, "$1");
}
function read(relative: string): string {
  return code(readFileSync(fileURLToPath(new URL(relative, import.meta.url)), "utf8"));
}

describe("the pane gates its roster reads on the capability (the wiring half)", () => {
  const viewSource = read("../conversation-shell/components/channel-view.tsx");
  const popoverSource = read("../agents/components/agent-info-popover.tsx");

  it("computes ONE roster target and hands it to BOTH roster reads", () => {
    // Desktop's test env is `node` (no jsdom/RTL), so the counter-example @Cindy
    // asked for — a DM mode not starting these query consumers — is asserted on
    // structure: the hooks receive the gated target, and both of them disable
    // their query AND their WS interest on `null`.
    expect(viewSource).toContain("rosterQueryTarget(");
    expect(viewSource).toContain("useChannelAgents(rosterTarget)");
    expect(viewSource).toContain("useChannelAgentRuntime(rosterTarget)");
    // The shape that shipped and the review rejected: membership alone starting
    // the reads, with the placement's capability only hiding the surfaces.
    expect(viewSource).not.toContain("useChannelAgents(gate.allowMemberEffects");
    expect(viewSource).not.toContain("useChannelAgentRuntime(gate.allowMemberEffects");
  });

  it("provides the capability once, at the pane boundary, from its own projection", () => {
    expect(viewSource).toContain("const paneCaps = paneCapabilities(paneMode)");
    expect(viewSource).toContain("<ConversationPaneCapabilitiesProvider value={paneCaps}>");
  });

  it("does not mount the management read chain where there is no roster", () => {
    // `AgentManagementActions` starts roster, runtime, conversation-ref and
    // workspace-member reads; it used to hide itself because a pair's roster
    // response came back without a row.
    expect(popoverSource).toContain("conversationId && paneCaps.hasRoster ?");
  });

  it("feeds the presence line the roster capability, not a presence-source enum", () => {
    // The placement decides which authority answers presence (#task:2123), but it
    // decides it from `hasRoster` — the same field that gates the reads — and never
    // from a second `agentPresence` source enum that could disagree with them.
    expect(popoverSource).toContain("hasRoster={paneCaps.hasRoster}");
    expect(popoverSource).not.toContain("paneCaps.agentPresence");
    expect(popoverSource).toContain("presenceAuthorityFor(hasRoster)");
  });

  it("reads a roster placement's presence from the runtime slot, not Availability", () => {
    // The #task:2123 bug in structural form: the roster arm's member case used to
    // fall through to `GlobalPresenceInline`, so the card contradicted the chip it
    // was opened from. The projection is now the member arm's only source, and
    // `useAgentPresence` must remain reachable ONLY from the global arm.
    expect(popoverSource).toContain("projectMemberRuntimeStatus(");
    expect(popoverSource).toContain("useChannelAgentRuntime(conversationId)");
    const rosterArmStart = popoverSource.indexOf("function RosterMemberPresence");
    expect(rosterArmStart).toBeGreaterThan(-1);
    const rosterArm = popoverSource.slice(
      rosterArmStart,
      popoverSource.indexOf("\nfunction ", rosterArmStart + 1),
    );
    expect(rosterArm).not.toContain("GlobalPresenceInline");
    expect(rosterArm).not.toContain("useAgentPresence");
    // And neither roster read may be defaulted: `= []` turns an unsettled read
    // into a confident answer for every member in the channel.
    expect(popoverSource).not.toContain("useChannelAgents(conversationId) ?? []");
    expect(popoverSource).not.toContain("data: runtime = []");
    expect(popoverSource).not.toContain("data: members = []");
  });

  it("mounts the thread pane INSIDE the inherited capability (#task:2098)", () => {
    // The thread pane is the one surface outside `ChannelView` that hands a
    // `conversationId` to the popover, which is what makes it the only place the
    // missing provider could surface. Containment is asserted, not adjacency: the
    // panel has to sit between the provider's tags, since a provider that merely
    // appears in the file is the bug all over again.
    const appSource =
      read("../conversation-shell/attached-pane.tsx") +
      read("../conversation-shell/build-conversation-panes.ts") +
      read("../conversation-shell/use-conversation-read-model.ts");
    const open = appSource.indexOf(
      "<ConversationPaneCapabilitiesProvider value={model.capabilities}>",
    );
    const close = appSource.indexOf("</ConversationPaneCapabilitiesProvider>", open);
    expect(open).toBeGreaterThan(-1);
    expect(close).toBeGreaterThan(open);
    expect(appSource.slice(open, close)).toContain("<ThreadPanel");
  });

  it("inherits from the column's own descriptor, not a re-guess", () => {
    // Acceptance #3: no `workspaceId === null`, no route sniffing, no local
    // boolean. Each column projects its capabilities from the descriptor it
    // resolved, and the split column's view renders that same descriptor.
    const appSource =
      read("../conversation-shell/conversation-column.ts") +
      read("../conversation-shell/build-conversation-panes.ts");
    expect(appSource).toContain("inheritedPaneCapabilities(descriptor)");
    expect(appSource).toContain("conversation: owner.descriptor");
  });
});
