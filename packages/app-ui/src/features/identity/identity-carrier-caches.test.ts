import { beforeEach, describe, expect, it, vi } from "vitest";
import { QueryClient, QueryObserver } from "@tanstack/react-query";
import type { ServerFrame } from "@silverhand/shared";
import { api } from "@/lib/api";
import { decideAgentSyncAction } from "@/features/agents/model/agent-sync";
import {
  agentsKey,
  clusterFetchEpochKey,
  clusterMessagesKey,
  mainlineLiveGapKey,
  messagesKey,
  threadFollowersKey,
  threadRootKey,
} from "@/features/channels/message-cache-core";
import {
  channelThreadsKey,
  channelThreadsUnreadKey,
} from "@/features/channels/use-channel-threads";
import { channelAttachmentsKey } from "@/features/channels/use-channel-attachments";
import { directPairsKey, directPairsQueryOptions } from "@/features/direct-pairs/hooks";
import { savedKey } from "@/features/messages/saved";
import { searchKey } from "@/features/search/hooks";
import { decideUserSyncAction } from "@/features/users/user-sync";
import {
  carrierIdsWithFacet,
  identityCarrierIdsFor,
  invalidateIdentityCarrierCaches,
} from "./identity-carrier-caches";

// Only `api` is replaced, so the pair list's re-read can be COUNTED.
vi.mock("@/lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/api")>();
  return { ...actual, api: vi.fn() };
});
const mockApi = vi.mocked(api);

/**
 * The registry's value is its SCOPING, not its inventory: one shared
 * enumeration must not make a human avatar change refetch host status / DM
 * favorites, nor an agent rename refetch the human directory (Cindy
 * #task:1703). These assert that boundary through the real query cache, so a
 * carrier added to the wrong group fails here.
 */
describe("identity-carrier caches", () => {
  const seed = (qc: QueryClient, key: readonly unknown[]) => {
    qc.setQueryData(key, { seeded: true });
  };

  /** Keys marked stale by an invalidation of `kind`. */
  const stalePaths = (kind: "agent" | "human"): string[] => {
    const qc = new QueryClient();
    const keys: readonly (readonly unknown[])[] = [
      ["conversation-members", "c1"],
      ["workspaces", "w1", "members"],
      ["workspaces", "w1", "channels"],
      ["activity", "all"],
      ["message-reactions", "m1"],
      ["tasks", { workspaceId: "w1" }],
      ["task-detail", "t1"],
      ["task-by-number", "w1", 7],
      ["agents"],
      ["favorites", "all"],
      ["host-status", "h1"],
      ["host-resources", "h1"],
      ["hosts"],
      ["users"],
      // #task:1792 — every channel-scoped read model that now embeds a
      // `MessageAuthorDto` snapshot, built from its REAL factory so a segment
      // rename breaks this test rather than silently dropping a family.
      messagesKey("c1"),
      clusterMessagesKey("c1", "root1"),
      threadRootKey("c1", "root1"),
      channelThreadsKey("c1"),
      channelThreadsUnreadKey("c1"),
      channelAttachmentsKey("c1"),
      savedKey,
      searchKey,
      // #task:2046 — the DM list resolves its whole label at read time, so it
      // carries the counterpart's CURRENT identity plus the lifecycle half (the
      // `deleted` arm + the derived `read_only`). Built from the real factory,
      // same discipline as the channel families above.
      directPairsKey,
      // Non-author channel caches that share the `['channels', cid, …]` prefix
      // (and, for the cluster arm, the `'cluster'` segment) — they must NOT be
      // reseated by an identity change.
      agentsKey("c1"),
      mainlineLiveGapKey("c1"),
      threadFollowersKey("c1", "root1"),
      clusterFetchEpochKey("c1", "root1"),
    ];
    for (const k of keys) seed(qc, k);
    invalidateIdentityCarrierCaches(qc, kind);
    return qc
      .getQueryCache()
      .findAll({ stale: true })
      .map((q) => JSON.stringify(q.queryKey))
      .sort();
  };

  it("reseats both-kind rosters, Activity, reactors and task dtos for either kind", () => {
    const shared = [
      '["activity","all"]',
      '["conversation-members","c1"]',
      '["direct-pairs"]',
      '["message-reactions","m1"]',
      '["task-by-number","w1",7]',
      '["task-detail","t1"]',
      '["tasks",{"workspaceId":"w1"}]',
      '["workspaces","w1","members"]',
    ];
    for (const kind of ["agent", "human"] as const) {
      expect(stalePaths(kind)).toEqual(expect.arrayContaining(shared));
    }
  });

  it("reseats every embedded-author read model, both kinds (#task:1792)", () => {
    // A hard-deleted actor's snapshot, a rename, and an avatar swap all only
    // reflect on these caches by refetch — none is keyed by the actor id.
    const authorCarriers = [
      JSON.stringify(messagesKey("c1")),
      JSON.stringify(clusterMessagesKey("c1", "root1")),
      JSON.stringify(threadRootKey("c1", "root1")),
      JSON.stringify(channelThreadsKey("c1")),
      JSON.stringify(channelThreadsUnreadKey("c1")),
      JSON.stringify(channelAttachmentsKey("c1")),
      JSON.stringify(savedKey),
      JSON.stringify(searchKey),
    ];
    for (const kind of ["agent", "human"] as const) {
      expect(stalePaths(kind)).toEqual(expect.arrayContaining(authorCarriers));
    }
  });

  it("does NOT reseat non-author channel caches sharing the prefix (#task:1792)", () => {
    // The cluster arm matches only `…, root, 'messages'`; `fetch-epoch` shares
    // the `'cluster'` segment but carries no author. `agents` / mainline
    // `live-gap` / `thread-followers` are pure affordance/roster caches.
    const nonAuthor = [
      JSON.stringify(agentsKey("c1")),
      JSON.stringify(mainlineLiveGapKey("c1")),
      JSON.stringify(threadFollowersKey("c1", "root1")),
      JSON.stringify(clusterFetchEpochKey("c1", "root1")),
    ];
    for (const kind of ["agent", "human"] as const) {
      const stale = stalePaths(kind);
      for (const key of nonAuthor) expect(stale).not.toContain(key);
    }
  });

  it("leaves the other kind's caches alone", () => {
    const agent = stalePaths("agent");
    expect(agent).toEqual(
      expect.arrayContaining([
        '["agents"]',
        '["favorites","all"]',
        '["host-resources","h1"]',
        '["host-status","h1"]',
      ]),
    );
    expect(agent).not.toContain('["users"]');

    const human = stalePaths("human");
    expect(human).toContain('["users"]');
    for (const agentOnly of ['["agents"]', '["host-status","h1"]', '["host-resources","h1"]']) {
      expect(human).not.toContain(agentOnly);
    }
  });

  it("never widens past the member list into a workspace's other lists", () => {
    for (const kind of ["agent", "human"] as const) {
      expect(stalePaths(kind)).not.toContain('["workspaces","w1","channels"]');
    }
    // `HostDto` carries no agent identity, so the host list itself is exempt.
    expect(stalePaths("agent")).not.toContain('["hosts"]');
  });

  it("exposes the per-kind carrier set for review", () => {
    expect(identityCarrierIdsFor("human")).toContain("users");
    expect(identityCarrierIdsFor("human")).toContain("favorites");
    expect(identityCarrierIdsFor("agent")).toContain("favorites");
    expect(identityCarrierIdsFor("agent")).not.toContain("users");
    // The embedded-author families are both-kind carriers (#task:1792).
    for (const kind of ["agent", "human"] as const) {
      expect(identityCarrierIdsFor(kind)).toEqual(
        expect.arrayContaining([
          "channel-author-projections",
          "saved",
          "search",
          // Either endpoint of a pair can be the counterpart (#task:2046).
          "direct-pairs",
        ]),
      );
    }
  });

  it("splits the identity facets, and only DM counterpart carriers are lifecycle (#task:2046)", () => {
    // The registry was presentation-flavored until the DM list arrived: its
    // `deleted` arm + derived `read_only` are the first LIFECYCLE identity, and
    // they fail toward "looks writable". These pins make the split enforced —
    // narrowing `direct-pairs` to presentation-only, or dropping it, reddens
    // here rather than shipping a composer that accepts input the send path
    // refuses.
    // Starred direct pairs embed the same counterpart (#task:2874).
    // The badge roll-up embeds no name, only whether the counterpart exists.
    expect(carrierIdsWithFacet("lifecycle")).toEqual([
      "direct-pairs",
      "attention-badges",
      "favorites",
    ]);
    expect(carrierIdsWithFacet("presentation")).toContain("direct-pairs");
    expect(carrierIdsWithFacet("presentation")).not.toContain("attention-badges");
    for (const kind of ["agent", "human"] as const) {
      for (const id of identityCarrierIdsFor(kind)) {
        if (id === "attention-badges") continue;
        expect(carrierIdsWithFacet("presentation")).toContain(id);
      }
    }
  });
});

/**
 * The DM list is an identity carrier, and the two axes' REAL decision seams have
 * to reach it (#task:2046).
 *
 * Why this is not covered by the registry cases above: those seed a key and read
 * the `stale` flag. What #2038 B2 changed is the COST model — the list is
 * cache-until-invalidated (`DIRECT_PAIRS_QUERY_POLICY.staleMs = Infinity`), so
 * "marked stale" is only half an answer. Under Infinity a mount never re-reads on
 * its own, which is exactly why the missing registration would have been
 * permanent for the session rather than "briefly old": the accidental
 * observer-mount refetch that used to paper over it is gone. So these count real
 * `/api/direct-pairs` reads, on both sides of the active/inactive split, driven by
 * the frames the backend actually publishes.
 *
 * The seams composed here are the bodies of `useAgentSync` / `useUserSync`
 * verbatim (decide → `invalidateIdentityCarrierCaches(qc, kind)`); the hook
 * wiring itself is pinned by `agent-sync.test.ts` / `user-sync.test.ts` and the
 * shell-link cases in `direct-pairs/hooks.test.ts`, and desktop's test env is
 * `node` (no RTL to mount them here).
 *
 * FIRST PROOF POINT (#2046's own acceptance): delete the `['direct-pairs']` entry
 * from `IDENTITY_CARRIERS` and every case in this describe goes red.
 */
describe("direct-pairs identity carrier — both axes, active and inactive", () => {
  const evt = (kind: string, payload: unknown): ServerFrame =>
    ({ type: "event", scope: "app", seq: 1, epoch: "e", kind, payload }) as ServerFrame;

  const client = () => new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const mountReader = (qc: QueryClient): (() => void) =>
    new QueryObserver(qc, directPairsQueryOptions()).subscribe(() => {});

  beforeEach(() => {
    mockApi.mockReset();
    mockApi.mockResolvedValue({ direct_pairs: [] });
  });

  it("an agent_changed frame re-reads the list while a reader is mounted", async () => {
    // The counterpart's `display_name` / `avatar_url` / `runtime` are CURRENT
    // values, so an agent rename must reach an open DM list.
    const qc = client();
    const off = mountReader(qc);
    await qc.fetchQuery(directPairsQueryOptions());
    expect(mockApi).toHaveBeenCalledTimes(1);

    const action = decideAgentSyncAction(evt("agent_changed", { agent_id: "a1" }));
    expect(action).toEqual({ kind: "invalidate" });
    invalidateIdentityCarrierCaches(qc, "agent");

    await vi.waitFor(() => expect(mockApi).toHaveBeenCalledTimes(2));
    off();
  });

  it("a user_changed frame re-reads it too — a human can be the counterpart", async () => {
    const qc = client();
    const off = mountReader(qc);
    await qc.fetchQuery(directPairsQueryOptions());
    expect(mockApi).toHaveBeenCalledTimes(1);

    const action = decideUserSyncAction(evt("user_changed", { user_id: "u1" }));
    expect(action).toEqual({ kind: "invalidate", userId: "u1" });
    invalidateIdentityCarrierCaches(qc, "human");

    await vi.waitFor(() => expect(mockApi).toHaveBeenCalledTimes(2));
    off();
  });

  it("a frame that lands while the list is unread is paid for on the next mount", async () => {
    // The inactive axis, and the one that matters most for a sidebar list: an
    // identity change arriving while DIRECT MESSAGES is closed (it renders
    // nothing when the viewer has no pairs, and the pane can be shut) must not
    // be a silent no-op. `invalidateQueries` ignores `staleTime`, so the entry is
    // marked without fetching and the next reader pays for it.
    const qc = client();
    await qc.fetchQuery(directPairsQueryOptions());
    expect(mockApi).toHaveBeenCalledTimes(1);

    invalidateIdentityCarrierCaches(qc, "agent");
    await Promise.resolve();
    expect(mockApi).toHaveBeenCalledTimes(1); // nothing mounted ⇒ marked, not fetched

    const off = mountReader(qc);
    await vi.waitFor(() => expect(mockApi).toHaveBeenCalledTimes(2));
    off();
  });

  it("a snapshot_required recovery on the app scope reaches it as well", async () => {
    // The identity axes' replay path: a dropped app-scope delta may have been
    // the `agent_changed` for this counterpart, so the recovery frame has to
    // reseat the same carriers. Distinct from `useDirectPairsSync`'s own
    // viewer-scope recovery, which converges the pair SET rather than identity.
    const qc = client();
    const off = mountReader(qc);
    await qc.fetchQuery(directPairsQueryOptions());

    const frame = {
      type: "snapshot_required",
      scope: "app",
      reason: "epoch_mismatch",
    } as unknown as ServerFrame;
    expect(decideAgentSyncAction(frame)).toEqual({ kind: "invalidate" });
    invalidateIdentityCarrierCaches(qc, "agent");

    await vi.waitFor(() => expect(mockApi).toHaveBeenCalledTimes(2));
    off();
  });
});
