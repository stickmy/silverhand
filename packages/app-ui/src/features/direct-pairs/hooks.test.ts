import { beforeEach, describe, expect, it, vi } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { QueryClient, QueryObserver } from "@tanstack/react-query";
import type { DirectPairListItem } from "@silverhand/shared";
import { api } from "@/lib/api";
import {
  DIRECT_PAIRS_QUERY_POLICY,
  decideDirectPairsInvalidation,
  decideDraftHandoff,
  directPairPaneProps,
  directPairsKey,
  directPairsQueryOptions,
  findAgentDirectPair,
  projectDirectPairAssignee,
  projectDirectPairPane,
  type DirectPairListQueryFacts,
  type DirectPairPaneState,
  type DraftHandoffFacts,
} from "./hooks";

// Only `api` is replaced so the query policy's cost can be COUNTED; everything
// else in `@/lib/api` stays real (nothing here depends on the rest, but a bare
// `vi.mock` would auto-mock `ApiError` too).
vi.mock("@/lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/api")>();
  return { ...actual, api: vi.fn() };
});

const mockApi = vi.mocked(api);

function pair(conversationId: string): DirectPairListItem {
  return {
    conversation_id: conversationId,
    counterpart: {
      status: "active",
      kind: "agent",
      id: "a-1",
      name: "alice",
      display_name: "Alice",
      avatar_url: null,
      runtime: "claude",
    },
    unread_count: 1,
    read_only: false,
    created_at: 1,
  };
}

/** Settled-successful facts: a read has answered and nothing is in flight. */
function settled(rows: readonly DirectPairListItem[]): DirectPairListQueryFacts {
  return { rows, isPending: false, isFetching: false, isError: false };
}

/** Source with comments removed. Load-bearing for the structural counts below: a
 *  probe that COMMENTS OUT a mount or a prop is the likeliest way these
 *  obligations die, and a raw `includes` would still find the call inside the
 *  comment and stay green. (Only `//` that does not follow a `:` is stripped, so a
 *  `scheme://` literal survives.) */
function code(source: string): string {
  return source.replace(/\/\*[\s\S]*?\*\//g, "").replace(/(^|[^:])\/\/.*$/gm, "$1");
}
function read(relative: string): string {
  return code(readFileSync(fileURLToPath(new URL(relative, import.meta.url)), "utf8"));
}

describe("findAgentDirectPair — existing line, or a draft", () => {
  /** A pair whose counterpart is a named agent. */
  function agentPair(conversationId: string, agentId: string): DirectPairListItem {
    return {
      ...pair(conversationId),
      counterpart: {
        status: "active",
        kind: "agent",
        id: agentId,
        name: agentId,
        display_name: "",
        avatar_url: null,
        runtime: "claude",
      },
    };
  }
  /** A pair whose agent is gone. The `deleted` arm carries only kind + id. */
  function deletedPair(conversationId: string, agentId: string): DirectPairListItem {
    return {
      ...pair(conversationId),
      counterpart: { status: "deleted", kind: "agent", id: agentId },
    };
  }

  it("finds the line the viewer already has with that agent", () => {
    const mine = agentPair("c-mine", "a-target");
    expect(findAgentDirectPair([agentPair("c-other", "a-other"), mine], "a-target")).toEqual(mine);
  });

  it("finds it even when the agent is gone — history beats a door that answers 404", () => {
    // The `deleted` arm still NAMES the endpoint. Falling through to the draft
    // route would offer to start a line that cannot be started, while hiding the
    // one that exists.
    const gone = deletedPair("c-gone", "a-target");
    expect(findAgentDirectPair([gone], "a-target")).toEqual(gone);
  });

  it("never matches a human counterpart that happens to share the id", () => {
    // Ids are per-kind, so `agent:a-1` and `human:a-1` can coexist; the caller
    // asks about an AGENT and a human row is a different line entirely.
    const human: DirectPairListItem = {
      ...pair("c-human"),
      counterpart: {
        status: "active",
        kind: "human",
        id: "a-target",
        name: "shiki",
        display_name: "shiki",
        avatar_url: null,
        runtime: null,
      },
    };
    expect(findAgentDirectPair([human], "a-target")).toBeNull();
  });

  it("returns null for an agent with no line, and for a cold list", () => {
    // Both read as "no pair" on purpose: the caller's fallback is the draft
    // route, whose first send is get-or-create — a stale miss costs a
    // converge-on-send, never a duplicate line.
    expect(findAgentDirectPair([agentPair("c-other", "a-other")], "a-target")).toBeNull();
    expect(findAgentDirectPair([], "a-target")).toBeNull();
  });
});

describe("decideDirectPairsInvalidation — which viewer frame re-reads the list", () => {
  it("re-reads on direct_pairs_changed", () => {
    expect(
      decideDirectPairsInvalidation({
        type: "event",
        kind: "direct_pairs_changed",
      }),
    ).toBe(true);
  });

  it("re-reads on snapshot_required — a replay gap can swallow the only announcement", () => {
    // The frame carries no `kind`. A brand-new pair is a row the client has no
    // other way to learn about, so a gap must re-read rather than wait.
    expect(decideDirectPairsInvalidation({ type: "snapshot_required" })).toBe(true);
  });

  it("ignores every OTHER viewer-scoped event kind", () => {
    // The viewer scope is shared with the workspace list, unread, presence and
    // the task board; this list must not refetch on their traffic.
    for (const kind of [
      "viewer_workspaces_changed",
      "workspace_channels_changed",
      "unread_updated",
      "task_changed",
      "message",
      "presence_updated",
    ]) {
      expect(decideDirectPairsInvalidation({ type: "event", kind })).toBe(false);
    }
  });

  it("ignores non-event frames and an event with no kind", () => {
    for (const type of ["ack", "subscribed", "pong", "error"]) {
      expect(decideDirectPairsInvalidation({ type, kind: "direct_pairs_changed" })).toBe(false);
    }
    expect(decideDirectPairsInvalidation({ type: "event" })).toBe(false);
  });
});

describe("projectDirectPairPane — what the pane may claim about the pair", () => {
  it("is idle when the route names no pair", () => {
    expect(projectDirectPairPane(null, settled([]))).toEqual({ kind: "idle" });
    // Idle even mid-fetch: nothing is being awaited ON BEHALF of a pane.
    expect(
      projectDirectPairPane(null, {
        rows: undefined,
        isPending: true,
        isFetching: true,
        isError: false,
      }),
    ).toEqual({ kind: "idle" });
  });

  it("resolves a row the current answer contains", () => {
    expect(projectDirectPairPane("c-pair", settled([pair("c-pair")]))).toEqual({
      kind: "ready",
      pair: pair("c-pair"),
    });
  });

  it("waits on the cold read instead of flashing the empty state", () => {
    expect(
      projectDirectPairPane("c-pair", {
        rows: undefined,
        isPending: true,
        isFetching: true,
        isError: false,
      }),
    ).toEqual({ kind: "resolving" });
  });

  it("THE ACCEPTANCE SEQUENCE: cached-empty → invalidated refetch → new row", () => {
    // A viewer whose list was fetched before the stranger's first DM. The
    // invalidation arrives; React Query RETAINS the empty rows and raises
    // `isFetching` while `isPending` stays false.
    const cachedEmpty = settled([]);
    expect(projectDirectPairPane("c-new", cachedEmpty)).toEqual({ kind: "absent" });

    const refetching: DirectPairListQueryFacts = {
      rows: [],
      isPending: false,
      isFetching: true,
      isError: false,
    };
    // The step that an `isPending`-only projection gets WRONG: it would report a
    // settled absence here and the pane would flash "Select a channel" before
    // the row lands.
    expect(projectDirectPairPane("c-new", refetching)).toEqual({
      kind: "resolving",
    });

    expect(projectDirectPairPane("c-new", settled([pair("c-new")]))).toEqual({
      kind: "ready",
      pair: pair("c-new"),
    });
  });

  it("calls a cold bad id absent once the read has answered — not a loader that never ends", () => {
    expect(projectDirectPairPane("c-nope", settled([pair("c-other")]))).toEqual({
      kind: "absent",
    });
  });

  it("reports a failed read as error, never as absence", () => {
    // Nothing was proven about the pair, and with `retry: 1` the client is not
    // going to keep trying: a terminal empty state would be a lie.
    expect(
      projectDirectPairPane("c-pair", {
        rows: undefined,
        isPending: false,
        isFetching: false,
        isError: true,
      }),
    ).toEqual({ kind: "error" });
    // Same when a refetch fails over rows that don't contain the id: retained
    // rows are the PREVIOUS answer, so absence is still unproven.
    expect(
      projectDirectPairPane("c-pair", {
        rows: [pair("c-other")],
        isPending: false,
        isFetching: false,
        isError: true,
      }),
    ).toEqual({ kind: "error" });
  });

  it("prefers a row we hold over both an in-flight refetch and a failed one", () => {
    // Precedence `ready > resolving > error`: the pane can render, so it must
    // not blank out just because the list is being revalidated or failed to be.
    expect(
      projectDirectPairPane("c-pair", {
        rows: [pair("c-pair")],
        isPending: false,
        isFetching: true,
        isError: false,
      }),
    ).toMatchObject({ kind: "ready" });
    expect(
      projectDirectPairPane("c-pair", {
        rows: [pair("c-pair")],
        isPending: false,
        isFetching: false,
        isError: true,
      }),
    ).toMatchObject({ kind: "ready" });
  });

  it("treats an in-flight retry as resolving, so a stale error never overrides a live attempt", () => {
    // Reachable shape: the query errored with no data, then something (an
    // invalidation, a remount) refetched it. The error is RETAINED while the new
    // fetch runs, so `isError` and `isFetching` are true together and
    // `isPending` is false.
    expect(
      projectDirectPairPane("c-pair", {
        rows: undefined,
        isPending: false,
        isFetching: true,
        isError: true,
      }),
    ).toEqual({ kind: "resolving" });
  });
});

describe("projectDirectPairAssignee — the DM task-assignee domain (#task:2041, @Cindy)", () => {
  // The counterpart the picker offers, over the five pane states × the counterpart
  // kinds. The load-bearing property is P1: the active counterpart is in the domain
  // even when it IS the current assignee (`isCurrent`), never dropped — dropping it
  // was the reported empty-dropdown bug for a DM assigned to its one agent.
  const activeAgentPane = (id: string, name = id, displayName = ""): DirectPairPaneState => ({
    kind: "ready",
    pair: {
      conversation_id: "c-pair",
      counterpart: {
        status: "active",
        kind: "agent",
        id,
        name,
        display_name: displayName,
        avatar_url: null,
        runtime: "claude",
      },
      unread_count: 0,
      read_only: false,
      created_at: 1,
    },
  });

  it("offers the active counterpart when it is NOT the current assignee", () => {
    expect(
      projectDirectPairAssignee(activeAgentPane("a-1", "alice", "Alice"), {
        type: "agent",
        id: "someone-else",
      }),
    ).toEqual({
      status: "ready",
      candidates: [
        { type: "agent", id: "a-1", name: "alice", displayName: "Alice", isCurrent: false },
      ],
    });
  });

  it("P1: STILL offers the counterpart when it is already the assignee, marked isCurrent", () => {
    // The empty-dropdown regression: excluding the current agent left a DM assigned
    // to its one counterpart with no candidate at all.
    expect(
      projectDirectPairAssignee(activeAgentPane("a-1", "alice", "Alice"), {
        type: "agent",
        id: "a-1",
      }),
    ).toEqual({
      status: "ready",
      candidates: [
        { type: "agent", id: "a-1", name: "alice", displayName: "Alice", isCurrent: true },
      ],
    });
  });

  it("falls back to the handle when the counterpart has no display name", () => {
    expect(
      projectDirectPairAssignee(activeAgentPane("a-1", "alice", ""), null).candidates[0],
    ).toMatchObject({ displayName: "alice" });
  });

  it("offers a HUMAN counterpart as a user candidate", () => {
    const humanPane: DirectPairPaneState = {
      kind: "ready",
      pair: {
        conversation_id: "c-pair",
        counterpart: {
          status: "active",
          kind: "human",
          id: "u-1",
          name: "shiki",
          display_name: "shiki",
          avatar_url: null,
          runtime: null,
        },
        unread_count: 0,
        read_only: false,
        created_at: 1,
      },
    };
    expect(projectDirectPairAssignee(humanPane, null)).toEqual({
      status: "ready",
      candidates: [
        { type: "user", id: "u-1", name: "shiki", displayName: "shiki", isCurrent: false },
      ],
    });
  });

  it("offers the viewer alongside the counterpart", () => {
    const viewer = { id: "u-me", name: "shiki", displayName: "Shiki" };
    expect(
      projectDirectPairAssignee(activeAgentPane("a-1", "alice", "Alice"), null, viewer),
    ).toEqual({
      status: "ready",
      candidates: [
        { type: "agent", id: "a-1", name: "alice", displayName: "Alice", isCurrent: false },
        { type: "user", id: "u-me", name: "shiki", displayName: "Shiki", isCurrent: false },
      ],
    });
    expect(
      projectDirectPairAssignee(
        activeAgentPane("a-1", "alice", "Alice"),
        { type: "user", id: "u-me" },
        viewer,
      ).candidates[1],
    ).toMatchObject({ isCurrent: true });
  });

  it("offers nothing for a DELETED counterpart", () => {
    const deletedPane: DirectPairPaneState = {
      kind: "ready",
      pair: {
        conversation_id: "c-pair",
        counterpart: { status: "deleted", kind: "agent", id: "a-1" },
        unread_count: 0,
        read_only: false,
        created_at: 1,
      },
    };
    expect(projectDirectPairAssignee(deletedPane, null)).toEqual({
      status: "ready",
      candidates: [],
    });
  });

  it('reports loading for the non-answers (resolving / idle), never "no agent"', () => {
    expect(projectDirectPairAssignee({ kind: "resolving" }, null)).toEqual({
      status: "loading",
      candidates: [],
    });
    expect(projectDirectPairAssignee({ kind: "idle" }, null)).toEqual({
      status: "loading",
      candidates: [],
    });
  });

  it('reports error for a failed read, never "no agent"', () => {
    expect(projectDirectPairAssignee({ kind: "error" }, null)).toEqual({
      status: "error",
      candidates: [],
    });
  });

  it('reports a settled-absent pair as ready-empty (an authoritative "nobody here")', () => {
    expect(projectDirectPairAssignee({ kind: "absent" }, null)).toEqual({
      status: "ready",
      candidates: [],
    });
  });
});

describe("directPairPaneProps — every state has a rendering (the consumer half)", () => {
  // @Cindy's B8 second round: the five-state projection is only worth having if
  // the shell can express five renderings. It could not — `absent` and `idle`
  // both fell through to "Select a channel", so turning `absent` into `idle`
  // changed the shipped UI by exactly nothing and the state was a test-only
  // classification. This mapping is where that is now falsifiable.
  const props = (state: DirectPairPaneState) => directPairPaneProps(state);

  it("renders nothing and waits for nothing when the route names no pair", () => {
    expect(props({ kind: "idle" })).toEqual({
      pair: null,
      resolving: false,
      loadFailed: false,
      notFound: false,
    });
  });

  it("carries the row through untouched", () => {
    const row = pair("c-pair");
    expect(props({ kind: "ready", pair: row })).toEqual({
      pair: row,
      resolving: false,
      loadFailed: false,
      notFound: false,
    });
  });

  it("asks the pane to wait, without claiming a failure or an absence", () => {
    expect(props({ kind: "resolving" })).toEqual({
      pair: null,
      resolving: true,
      loadFailed: false,
      notFound: false,
    });
  });

  it("reports a failed read as a failure only", () => {
    expect(props({ kind: "error" })).toEqual({
      pair: null,
      resolving: false,
      loadFailed: true,
      notFound: false,
    });
  });

  it("gives a SETTLED absence its own flag — not idle, not error", () => {
    // `/chat/d/<bad or no-longer-visible id>`: the route names a conversation
    // and the authority answered "not yours". Sharing `idle`'s rendering would
    // tell the viewer they chose nothing.
    expect(props({ kind: "absent" })).toEqual({
      pair: null,
      resolving: false,
      loadFailed: false,
      notFound: true,
    });
  });

  it("maps the five states to five DISTINCT props — no two collapse", () => {
    // The property that makes the projection load-bearing rather than
    // decorative: if any two states rendered the same, one of them could be
    // deleted with no observable effect.
    const states: DirectPairPaneState[] = [
      { kind: "idle" },
      { kind: "resolving" },
      { kind: "ready", pair: pair("c-pair") },
      { kind: "error" },
      { kind: "absent" },
    ];
    const rendered = states.map((s) => JSON.stringify(props(s)));
    expect(new Set(rendered).size).toBe(states.length);
  });
});

describe("decideDraftHandoff — when the draft pane may leave for the real pair", () => {
  // @Cindy's #task:2039 review, finding 2: the pane holds the only copy of two
  // things — the unsent text and the unread failure — so an automatic handoff is
  // a data-loss path, not a navigation detail. Four cells, all reachable.
  const facts = (over: Partial<DraftHandoffFacts> = {}): DraftHandoffFacts => ({
    existingPairId: "c-pair",
    draftDirty: false,
    sendPending: false,
    sendFailed: false,
    ...over,
  });

  it("stays while the viewer has no pair with this agent — that is the whole route", () => {
    expect(decideDraftHandoff(facts({ existingPairId: null }))).toEqual({
      kind: "stay",
    });
    // Including while typing, sending and after a failure: with no pair there is
    // nowhere to hand over to.
    expect(
      decideDraftHandoff({
        existingPairId: null,
        draftDirty: true,
        sendPending: true,
        sendFailed: true,
      }),
    ).toEqual({ kind: "stay" });
  });

  it("EMPTY: hands over to the pair that appeared underneath an untouched pane", () => {
    // The agent opened the line from its side. Nothing is lost, and the pair's
    // own route is the same interface with the history.
    expect(decideDraftHandoff(facts())).toEqual({
      kind: "handoff",
      conversationId: "c-pair",
    });
  });

  it("DIRTY: keeps unsent text instead of navigating away from it", () => {
    // The send is get-or-create, so staying costs nothing: their retry converges
    // on the very same pair.
    expect(decideDraftHandoff(facts({ draftDirty: true }))).toEqual({
      kind: "stay",
    });
  });

  it("PENDING: never moves the pane out from under an unsettled send", () => {
    // That send's own ack navigates with the conversation id the backend chose.
    expect(decideDraftHandoff(facts({ sendPending: true }))).toEqual({
      kind: "stay",
    });
  });

  it("POST-MINT ERROR: a pair exists AND the send failed → stay, so the reason is readable", () => {
    // The window #task:2036 accepts: initiation minted and announced the pair,
    // then message persistence failed. The pair now exists, so an
    // `existingPairId`-only rule would jump to an empty line and take the failure
    // off screen. Also with the draft cleared — `sendFailed` alone must hold.
    expect(decideDraftHandoff(facts({ sendFailed: true }))).toEqual({
      kind: "stay",
    });
    expect(decideDraftHandoff(facts({ sendFailed: true, draftDirty: true }))).toEqual({
      kind: "stay",
    });
  });

  it("carries the pair id through untouched — the pane never invents a destination", () => {
    expect(decideDraftHandoff(facts({ existingPairId: "c-other" }))).toEqual({
      kind: "handoff",
      conversationId: "c-other",
    });
  });
});

describe("the draft pane is per-agent, and defers to the pure handoff rule", () => {
  const paneSource = read("./direct-pair-draft-pane.tsx");
  const appSource = read("../../app.tsx");
  const contentSource =
    read("../conversation-shell/primary-pane-content.tsx") +
    read("../conversation-shell/build-conversation-panes.ts");
  const controllerSource = read("../conversation-shell/use-conversation-read-model.ts");
  const signalsSource = read("../app-shell/application-signals.ts");

  it("remounts on agent identity — a draft written to A can never be sent to B", () => {
    // @Cindy's finding 1: without the key, `/chat/d/new/A → /new/B` re-uses the
    // instance and carries `draft` plus the mutation state across agents.
    expect(contentSource).toContain("key={`${model.props.target.kind}:${model.props.target.id}`}");
  });

  it("takes the handoff decision from the seam, not from its own conditions", () => {
    expect(paneSource).toContain("decideDraftHandoff({");
    // The shape the review rejected: an effect that navigates on the pair's
    // existence, gated only by whether a send is in flight.
    expect(paneSource).not.toContain("initiate.isPending) return");
  });
});

describe("the live-discovery subscription is mounted once, by the session", () => {
  const hooksSource = read("./hooks.ts");
  const appSource = read("../../app.tsx");
  const contentSource =
    read("../conversation-shell/primary-pane-content.tsx") +
    read("../conversation-shell/build-conversation-panes.ts");
  const controllerSource = read("../conversation-shell/use-conversation-read-model.ts");
  const signalsSource = read("../app-shell/application-signals.ts");
  const shellSource = read("../app-shell/effects.ts");
  const panelSource = read("../../pages/chat/sidebar/direct-messages-panel.tsx");

  /** The body of a top-level `function <name>(`, up to the next top-level
   *  declaration. Lets a count be attributed to ONE component instead of to a
   *  2000-line module — `app.tsx` holds both the pre-auth gates and the shell. */
  function topLevelFunction(source: string, name: string): string {
    const start = source.indexOf(`function ${name}(`);
    expect(start).toBeGreaterThan(-1);
    const rest = source.slice(start + 1);
    const end = rest.search(/\n(?:export |function |const |class )/);
    return end === -1 ? rest : rest.slice(0, end);
  }

  it("the comment stripper actually strips — otherwise the counts below prove nothing", () => {
    expect(code("a\n// useDirectPairsSync();\nb")).not.toContain("useDirectPairsSync");
    expect(code("/* useDirectPairsSync(); */")).not.toContain("useDirectPairsSync");
    expect(code("const u = 'silverhand://agent/alice';")).toContain("silverhand://agent/alice");
  });

  it("the function slicer actually slices — a neighbour's body must not count", () => {
    const src = "function A() {\n  mount();\n}\nfunction B() {\n  mount();\n}\n";
    expect(topLevelFunction(src, "A").split("mount()").length - 1).toBe(1);
  });

  // @Cindy's B6 second round: the chain is only as strong as its weakest link.
  // Pinning `useAppShellGlobalSync`'s BODY proves the helper subscribes; it says
  // nothing about the shell still CALLING the helper. Her probe commented out
  // `useAppShellGlobalSync()` in App — which deletes every viewer-scoped
  // subscription from the shipped tree, so a stranger's first DM can never be
  // discovered — and the suite stayed green. All three links are pinned now.
  it("link 1: the post-auth shell calls the global-sync helper exactly once", () => {
    // `RegisteredAppShell` is the post-auth component by construction: `App`
    // returns it only after the loading, unreachable, epoch and registration
    // gates have each returned first.
    const shellComponent = topLevelFunction(appSource, "RegisteredAppShell");
    expect(shellComponent.split("useApplicationSignals()").length - 1).toBe(1);
    expect(
      topLevelFunction(signalsSource, "useApplicationSignals").split("useAppShellGlobalSync()")
        .length - 1,
    ).toBe(1);
    // And nowhere else in App — a second call would double every invalidation,
    // and a call in a pre-auth branch would subscribe with no viewer.
    expect(appSource.split("useApplicationSignals()").length - 1).toBe(1);
    expect(signalsSource.split("useAppShellGlobalSync()").length - 1).toBe(1);
    // The shell has to actually be mounted for any of this to run.
    expect(appSource.split("<RegisteredAppShell").length - 1).toBe(1);
  });

  it("link 2: the helper arms the direct-pair sync exactly once", () => {
    // The row must appear while the viewer is looking at ANY section, so the
    // obligation belongs to the session, not to the surface that renders the
    // list. Exactly once, because every mount invalidates the same key per
    // frame.
    expect(shellSource.split("useDirectPairsSync()").length - 1).toBe(1);
  });

  it("link 3: the section that renders the list arms NOTHING", () => {
    // A stranger's first DM has to arrive when DIRECT MESSAGES is absent from
    // the tree (it renders nothing when the viewer has no pairs) — a
    // subscription owned by that section could not discover the first row.
    expect(panelSource).not.toContain("useDirectPairsSync");
  });

  it("gates the viewer scope and routes the decision at the one key", () => {
    expect(hooksSource).toContain("subscribeViewerScope()");
    expect(hooksSource).toContain("isCurrentViewerScope(frame.scope)");
    expect(hooksSource).toContain("decideDirectPairsInvalidation(frame)");
    expect(hooksSource).toContain("queryKey: directPairsKey");
  });

  it("the pane consumes the pure mapping, and its settled absence reaches the view", () => {
    // The consumer direction for B8: the shell must not re-derive the states
    // (that is how `absent` got flattened), and the flag it derives has to be
    // handed to the pane rather than dropped.
    expect(controllerSource).toContain("directPairPaneProps(");
    expect(contentSource).toContain("notFound: read.directPairNotFound");
    expect(read("../conversation-shell/components/channel-view.tsx")).toContain("props.notFound");
  });
});

/**
 * What one more READER of the list costs (#task:2038 B2, @Cindy).
 *
 * The list used to be read by a handful of session-level surfaces; #task:2038
 * made it the identity authority for `#conv:<id>`, so a message stream mounts an
 * observer PER CHIP. Under React Query's default `staleTime: 0` every value is
 * stale on arrival and a stale query refetches WHEN A NEW OBSERVER MOUNTS, so
 * scrolling a busy conversation would fire `/api/direct-pairs` per chip — deduped
 * only for mounts that land while a request happens to be in flight. The claim
 * "adding this to every chip doesn't multiply network calls" was therefore false
 * as a matter of Query policy, not of intent.
 *
 * These cases hold the two halves of the fix together: mounting is free, AND the
 * invalidation ladder that owns freshness still works (a `staleTime` that also
 * silenced `invalidateQueries` would have traded a request storm for a list that
 * never updates).
 *
 * FALSIFIER: set `DIRECT_PAIRS_QUERY_POLICY.staleMs` to `0` — the two
 * observer-mount cases go red and the invalidation case stays green.
 */
describe("DIRECT_PAIRS_QUERY_POLICY — one more reader costs no request", () => {
  function client(): QueryClient {
    return new QueryClient({ defaultOptions: { queries: { retry: false } } });
  }
  /** A live observer on the list, mounted exactly the way a reader hook does. */
  function mountReader(qc: QueryClient): () => void {
    const observer = new QueryObserver(qc, directPairsQueryOptions());
    return observer.subscribe(() => {});
  }

  beforeEach(() => {
    mockApi.mockReset();
    mockApi.mockResolvedValue({ direct_pairs: [] });
  });

  it("the first read fetches once; two more readers mounting fetch nothing", async () => {
    const qc = client();
    await qc.fetchQuery(directPairsQueryOptions());
    expect(mockApi).toHaveBeenCalledTimes(1);

    const readers = [mountReader(qc), mountReader(qc)];
    await Promise.resolve();

    expect(mockApi).toHaveBeenCalledTimes(1);
    for (const off of readers) off();
  });

  it("a reader that unmounts and mounts again still fetches nothing", async () => {
    // A virtualized stream does exactly this as chips scroll in and out of the
    // window, which is why "one observer is already resident in App" does not
    // bound the cost on its own.
    const qc = client();
    await qc.fetchQuery(directPairsQueryOptions());
    mountReader(qc)();
    const off = mountReader(qc);
    await Promise.resolve();

    expect(mockApi).toHaveBeenCalledTimes(1);
    off();
  });

  it("the invalidation owner still re-reads it", async () => {
    // `invalidateQueries` ignores `staleTime`, so `useDirectPairsSync`
    // (`direct_pairs_changed` / `snapshot_required`) and the first-send mutation
    // remain the freshness carriers — this is the half that keeps Infinity honest.
    const qc = client();
    const off = mountReader(qc);
    await qc.fetchQuery(directPairsQueryOptions());
    expect(mockApi).toHaveBeenCalledTimes(1);

    await qc.invalidateQueries({ queryKey: directPairsKey });

    expect(mockApi).toHaveBeenCalledTimes(2);
    off();
  });

  it("an invalidation while NOBODY is reading still re-reads on the next mount", async () => {
    // The composition that makes cache-until-invalidated safe for a row whose
    // freshness comes from someone else's event: with `staleTime: Infinity` a
    // mount never guesses, but an INVALIDATED entry is stale regardless of
    // `staleTime`, so a carrier that fires while the surface is closed is not
    // lost — the next reader to mount pays for it. Without this, an invalidation
    // aimed at an inactive list would be a silent no-op and the row would stay
    // at its old value for the rest of the session.
    const qc = client();
    await qc.fetchQuery(directPairsQueryOptions());
    expect(mockApi).toHaveBeenCalledTimes(1);

    await qc.invalidateQueries({ queryKey: directPairsKey });
    expect(mockApi).toHaveBeenCalledTimes(1); // nothing mounted ⇒ marked, not fetched

    const off = mountReader(qc);
    await vi.waitFor(() => expect(mockApi).toHaveBeenCalledTimes(2));
    off();
  });

  it("every reader goes through the one options factory", () => {
    // A second `useQuery` on this key with its own freshness would restore the
    // mount refetch invisibly: whichever hook mounts first owns the entry, so
    // neither call site would look wrong.
    const hooksSource = read("./hooks.ts");
    expect(hooksSource).toContain("useQuery<DirectPairListResponse>(directPairsQueryOptions())");
    expect(hooksSource.split("staleTime:").length - 1).toBe(1);
    expect(hooksSource).toContain("staleTime: DIRECT_PAIRS_QUERY_POLICY.staleMs");
    // The policy is a value, not a literal buried in the query config.
    expect(DIRECT_PAIRS_QUERY_POLICY.staleMs).toBe(Infinity);
  });
});
