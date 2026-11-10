import type { QueryClient } from "@tanstack/react-query";
import { activityKey, attentionBadgesKey } from "@/features/attention/hooks";
import { messageReactorsKeyPrefix } from "@/features/channels/reaction-hooks";
import { CONVERSATION_MEMBERS_KEY_PREFIX } from "@/features/conversations/use-conversation-members";
import { conversationLatestKey } from "@/features/conversations/latest/use-conversation-latest";
import { directPairsKey } from "@/features/direct-pairs/hooks";
import { favoritesKey } from "@/features/workspaces/favorites";
import { hostResourcesKeyPrefix, hostStatusKeyPrefix } from "@/features/computers/hooks/hooks";
import { savedKey } from "@/features/messages/saved";
import { searchKey } from "@/features/search/hooks";
import { usersKey } from "@/features/users/hooks";
import { isWorkspaceMembersQueryKey } from "@/features/workspaces/hooks";

/**
 * The channel-scoped read models whose cached rows now embed a send-time
 * {@link MessageAuthorDto} snapshot (`display_name` + the `?v=updated_at` avatar
 * url), so a rename / avatar change / hard-delete must reseat them (#task:1792):
 *   - `['channels', cid, 'messages']`            mainline window (+ its inline
 *                                                `reply_previews_by_root` authors)
 *   - `['channels', cid, 'cluster', root, …]`    one reply cluster's window
 *   - `['channels', cid, 'thread-root', root]`   the thread pane's root row
 *   - `['channels', cid, 'threads-list', …]`     Threads All + followed-unread
 *                                                (root_author + repliers)
 *   - `['channels', cid, 'attachments-list', …]` attachment source author
 *
 * A structural match rather than a factory import because the channel families
 * vary by `cid` (no static left-anchored prefix reaches them). The boundary
 * test builds every key from its real factory (`messagesKey` / `clusterMessagesKey`
 * / `threadRootKey` / `channelThreadsKey` / `channelAttachmentsKey`), so a segment
 * rename fails the test instead of silently dropping a family. The cluster arm
 * matches only its `…, root, 'messages'` window, NOT the `fetch-epoch` / `live-gap`
 * siblings that share the `'cluster'` segment; other non-author channel caches
 * (`agents` / `thread-followers` / mainline `live-gap`) carry no snapshot.
 */
export function isChannelAuthorCarrierKey(key: readonly unknown[]): boolean {
  if (key[0] !== "channels") return false;
  const family = key[2];
  // The cluster window is `['channels', cid, 'cluster', rootId, 'messages']`;
  // its `fetch-epoch` / `live-gap` siblings share the `'cluster'` segment but
  // carry no author, so gate on the trailing `'messages'`.
  if (family === "cluster") return key[4] === "messages";
  return (
    family === "messages" ||
    family === "thread-root" ||
    family === "threads-list" ||
    family === "attachments-list"
  );
}

/**
 * Whose profile changed. The two actor kinds are isomorphic in the domain but
 * NOT in what they stale: an agent's `display_name` is denormalized into host
 * status and DM favorites, a human's into the user directory, and both into
 * every roster / Activity / reaction / task read model.
 */
export type IdentitySubjectKind = "agent" | "human";

/**
 * WHICH identity a carrier holds — kept separate because the two facets fail in
 * opposite directions and a future change must not conflate them (#task:2046):
 *  - `presentation` — a denormalized `display_name` / `avatar_url` / an agent's
 *    `runtime`. A stale one shows the wrong name; cosmetic, self-correcting on
 *    any convergence.
 *  - `lifecycle` — a projection of the subject's EXISTENCE: a `deleted` arm, or a
 *    write affordance derived from one (`DirectPairCounterpart.read_only`). A
 *    stale one fails toward "looks writable" — the composer accepts input the
 *    send path then refuses — so dropping its coverage is not a cosmetic
 *    regression. Naming it apart from `presentation` is what stops a change that
 *    "only touches the display layer" from quietly narrowing a carrier off it.
 *
 * The whole registry was presentation-flavored until the DM list (which carries
 * BOTH) arrived. The field is enforced, not decorative: `carrierIdsWithFacet`
 * exposes it so a test reddens if the DM list ever loses `lifecycle`.
 */
export type IdentityFacet = "presentation" | "lifecycle";

/** The filter shape `QueryClient.invalidateQueries` accepts. */
type InvalidateFilter = NonNullable<Parameters<QueryClient["invalidateQueries"]>[0]>;

interface IdentityCarrier {
  /** Stable slug — the review handle for "is this carrier covered?". */
  id: string;
  /** Which subject kinds' profile writes can stale this cache. Scoping is the
   *  point of the registry: a human avatar change must not refetch host status
   *  or DM favorites, and an agent rename must not refetch the human
   *  directory (Cindy #task:1703 boundary). */
  kinds: readonly IdentitySubjectKind[];
  /** Which identity facets the cached rows embed (see {@link IdentityFacet}).
   *  A property of the EMBEDDED VALUE, so it is declared here beside the carrier
   *  rather than inferred from the key. */
  facets: readonly IdentityFacet[];
  filter: InvalidateFilter;
}

/**
 * Every renderer cache that carries a DENORMALIZED identity snapshot
 * (`display_name` / `avatar_url` / an agent's `runtime`, resolved server-side
 * into someone else's read model), grouped by whose profile can stale it.
 *
 * ALSO IN SCOPE, and the wording matters (#task:2046): a projection derived from
 * the subject's EXISTENCE — a `deleted` arm, or a write affordance computed from
 * one — is carried identity too, because the terminal `agent_changed` /
 * `user_changed` frame IS the delete (`application/delete-agent.ts` publishes it
 * post-commit; there is no separate `*_deleted` event). A read model that embeds
 * name and no avatar still belongs here, so "does it show a name or an avatar?"
 * is NOT the admission question. Which facets a given row carries is a property
 * of the EMBEDDED VALUE, not of the cache key; today it is declared on the
 * renderer carrier entry beside its `filter` (the inventory is honestly a
 * hand-kept registration, see below). Deriving `facets` by construction from the
 * shared response schema — so a schema that grows a `deleted` arm can't be
 * registered as presentation-only — is #task:2047's job and is NOT implemented
 * yet; this registry does not pre-assume it.
 *
 * Why a registry instead of per-consumer invalidation: the actor-scoped signals
 * (`agent_changed`, and #task:1702's user-profile event) carry only an id.
 * These caches are keyed by conversation / workspace / host / message — never
 * by the actor — so no key prefix derived from the event can reach them. The
 * enumeration IS the contract; a new read model that embeds any of the above
 * belongs here, in the group matching whose identity it embeds.
 *
 * Invalidate, never patch: the identity fields are backend-derived (the avatar
 * `?v=updated_at` cache-buster among them), so re-reading is the only honest
 * convergence. Inactive queries are just marked stale — they refetch on next
 * mount — so a broad prefix costs nothing while the surface is closed.
 *
 * Deliberately NOT here:
 *   - `['me']` — the viewer's own record is IPC-backed (`getAuthState()`), and
 *     main answers from its in-memory session snapshot, so invalidation replays
 *     the same stale dto. The self arm needs a fresh `GET /api/me` (which also
 *     reseats main's snapshot on its way through the proxy —
 *     `main/profile-session-sync.ts`). Local writes already reseat both
 *     (#task:1701); only a REMOTE edit needs that read (#task:1702).
 *   - history `system_message` metadata and notification `trigger_name` —
 *     event-time snapshots by design; they must keep reading as of the event.
 */
const IDENTITY_CARRIERS: readonly IdentityCarrier[] = [
  // ── both kinds ────────────────────────────────────────────────────────────
  {
    // `AgentMemberDto` / `HumanMemberDto` both embed name + avatar; every
    // cached roster, not just the visible channel's (#task:1701).
    id: "conversation-members",
    kinds: ["agent", "human"],
    facets: ["presentation"],
    filter: { queryKey: CONVERSATION_MEMBERS_KEY_PREFIX },
  },
  {
    // `WorkspaceMembersResponse` = AgentDto[] + WorkspaceHumanMemberDto[]
    // (a UserDto extension), cached per workspace.
    id: "workspace-members",
    kinds: ["agent", "human"],
    facets: ["presentation"],
    filter: { predicate: (q) => isWorkspaceMembersQueryKey(q.queryKey) },
  },
  {
    // TWO server-resolved faces per list, both invalidated by the one entry (the
    // registration is per CACHE, and the subject kinds are the same):
    //   - the anchor message's sender (`MessageAuthorDto`);
    //   - a `direct_pair` row's COUNTERPART (#task:2115) — a LIVE join on the
    //     endpoint's principal, so a rename / avatar change / hard-delete of the
    //     other endpoint must reach this list too, exactly as it reaches the DM
    //     list itself.
    // Resolved server-side by design: the list is cross-workspace AND spans a
    // workspace-less placement, so the renderer has no directory that covers
    // every face it draws.
    id: "activity",
    kinds: ["agent", "human"],
    facets: ["presentation"],
    filter: { queryKey: activityKey() },
  },
  {
    // Every channel-scoped read model that embeds a `MessageAuthorDto` snapshot
    // (see {@link isChannelAuthorCarrierKey}): mainline + reply previews, reply
    // clusters, the thread-pane root row, the Threads list, and the attachments
    // list. The snapshot outlives the actor (`author_id` has no FK), so a rename
    // / avatar / hard-delete only reflects on these cached authors by refetch.
    id: "channel-author-projections",
    kinds: ["agent", "human"],
    // The stored `author_name` outlives a deleted actor (it is a SEND-TIME
    // snapshot, not a live join), so a hard-delete only ever changes the name/
    // avatar these rows show, never turns them into a `deleted` state —
    // presentation-only.
    facets: ["presentation"],
    filter: { predicate: (q) => isChannelAuthorCarrierKey(q.queryKey) },
  },
  {
    // `ConversationLatest.sender` — the Recent sidebar's preview author.
    id: "conversation-latest",
    kinds: ["agent", "human"],
    facets: ["presentation"],
    filter: { queryKey: conversationLatestKey },
  },
  {
    // `SavedMessageDto.message.author` — the saved row embeds the whole message,
    // so it carries the same mutable author snapshot as the byline. That
    // snapshot is now the row's ONLY identity carrier (#task:2064 deleted the
    // live-FK `agent_name` that used to sit beside it).
    id: "saved",
    kinds: ["agent", "human"],
    facets: ["presentation"],
    filter: { queryKey: savedKey },
  },
  {
    // `SearchResultDto.message.author` — a message hit embeds the full message,
    // hence the same author snapshot. Workspace-global key, so a plain prefix.
    id: "search",
    kinds: ["agent", "human"],
    facets: ["presentation"],
    filter: { queryKey: searchKey },
  },
  {
    // `ReactionReactorDto.display_name`. Fetched only while a chip is hovered,
    // but with `staleTime: 10_000` a reopen inside that window replays the old
    // name. Not re-resolved by id: reactors can include actors outside the
    // viewer's own directories, and the server join is the only complete
    // source.
    id: "message-reactors",
    kinds: ["agent", "human"],
    facets: ["presentation"],
    filter: { queryKey: messageReactorsKeyPrefix },
  },
  {
    // `DirectPairCounterpart` — the row's whole label is resolved at read time
    // (a `direct_pair` stores no title), so this list carries the counterpart's
    // CURRENT `display_name` / `avatar_url` / `runtime`, "exactly as
    // `MessageAuthorDto`'s active arm defines them" (its own docblock) — and
    // that dto's carrier is `channel-author-projections` above. Registered for
    // both kinds because either endpoint can be the counterpart.
    //
    // NOT droppable as cosmetic (#task:2046): the same row carries the
    // subject-lifecycle half — the `deleted` arm and the `read_only` survivor
    // state derived from it — and THAT half fails in the "looks writable"
    // direction (the composer accepts input, the send path refuses). Both halves
    // ride one frame here: the terminal `agent_changed` / `user_changed` IS the
    // delete, so no second event is needed. Invalidation is also the only
    // honest convergence for `read_only` — it is derived from the endpoints
    // server-side, never patched client-side.
    //
    // Load-bearing since #task:2038 made this list cache-until-invalidated
    // (`DIRECT_PAIRS_QUERY_POLICY.staleMs = Infinity`): with no time dimension
    // of staleness left, an unregistered carrier stays stale for the whole
    // session instead of being accidentally reseated on the next mount.
    id: "direct-pairs",
    kinds: ["agent", "human"],
    // The ONLY carrier that holds both today: the active arm's name/avatar/
    // runtime (presentation) AND the `deleted` arm + derived `read_only`
    // (lifecycle). Narrowing this to `['presentation']` is the exact regression
    // the split exists to catch.
    facets: ["presentation", "lifecycle"],
    filter: { queryKey: directPairsKey },
  },
  {
    // The roll-up's `direct_pairs` term skips pairs whose counterpart is
    // deleted, and that delete arrives only as `agent_changed` / `user_changed`.
    id: "attention-badges",
    kinds: ["agent", "human"],
    facets: ["lifecycle"],
    filter: { queryKey: attentionBadgesKey() },
  },
  // Task DTOs carry THREE identity snapshots — `assignee.name`, `creator.name`
  // and `last_editor.name` in the detail. All three are live joins re-resolved on
  // read, so invalidating the task caches converges them together; re-resolving
  // only the assignee by id would leave two freshness classes inside one dto
  // (Cindy #task:1703).
  //
  // `['tasks']` is the ROOT, so this one entry covers all three board faces —
  // column pages, counts, facets. It got MORE load-bearing when the board became
  // the only task read (#task:1885/#task:1892): the person filter options are now
  // a server facet read that carries its own `name` snapshots, where they used to
  // be aggregated client-side off the rows in hand.
  {
    id: "task-lists",
    kinds: ["agent", "human"],
    facets: ["presentation"],
    filter: { queryKey: ["tasks"] },
  },
  {
    id: "task-detail",
    kinds: ["agent", "human"],
    facets: ["presentation"],
    filter: { queryKey: ["task-detail"] },
  },
  {
    id: "task-by-number",
    kinds: ["agent", "human"],
    facets: ["presentation"],
    filter: { queryKey: ["task-by-number"] },
  },

  // ── agent-only ────────────────────────────────────────────────────────────
  {
    // The agent directory + every per-agent detail subtree. Avatars come along
    // for free: the object-url effect keys on the `?v=updated_at` url.
    id: "agents",
    kinds: ["agent"],
    facets: ["presentation"],
    filter: { queryKey: ["agents"] },
  },
  {
    // A favorite row snapshots its PARENT's identity: `read-model/favorites.ts`
    // joins `agents.display_name / name` into `parent_title`, and
    // `FavoritesPanel` renders that field directly instead of re-resolving by
    // id (Cindy #task:1703; fields re-scoped to the parent by #task:1888, and
    // the parent avatar/runtime fields deleted once the row led with `#`). Since
    // a STARRED row no longer appears under its parent, this invalidation is the
    // ONLY convergence path for that label — do not drop it.
    // Starred direct pairs (#task:2874) also embed the DM counterpart — either
    // kind, with its `deleted` arm — so the row converges like `direct-pairs`.
    id: "favorites",
    kinds: ["agent", "human"],
    facets: ["presentation", "lifecycle"],
    filter: { queryKey: favoritesKey },
  },
  {
    // `AgentOnHostDto.display_name` in each computer's status snapshot.
    id: "host-status",
    kinds: ["agent"],
    facets: ["presentation"],
    filter: { queryKey: hostStatusKeyPrefix },
  },
  {
    // `HostAgentResource.display_name` in each computer's ownership list.
    // `HostDto` itself carries no agent identity, so `['hosts']` / `['host',id]`
    // are intentionally absent.
    id: "host-resources",
    kinds: ["agent"],
    facets: ["presentation"],
    filter: { queryKey: hostResourcesKeyPrefix },
  },

  // ── human-only ────────────────────────────────────────────────────────────
  {
    // The global human directory. Also the resolver behind chat authors,
    // mention chips and sender filters, so this one entry converges every
    // surface that resolves humans by id.
    id: "users",
    kinds: ["human"],
    facets: ["presentation"],
    filter: { queryKey: usersKey },
  },
];

/** Carrier slugs that apply to one subject kind. Exported for the boundary
 *  test — the registry's value is its scoping, so that is what gets pinned. */
export function identityCarrierIdsFor(kind: IdentitySubjectKind): readonly string[] {
  return IDENTITY_CARRIERS.filter((c) => c.kinds.includes(kind)).map((c) => c.id);
}

/** Carrier slugs that declare a given identity facet. Exported so a test can
 *  pin that the DM list keeps its `lifecycle` coverage (and that the rest of the
 *  registry stays presentation-only until something else earns lifecycle) —
 *  #task:2046. This is what makes the facet split enforced rather than a label. */
export function carrierIdsWithFacet(facet: IdentityFacet): readonly string[] {
  return IDENTITY_CARRIERS.filter((c) => c.facets.includes(facet)).map((c) => c.id);
}

/**
 * Reseat every denormalized copy of one actor kind's identity.
 *
 * Called from the WS consumers that own each axis — `useAgentSync` for
 * `agent_changed`, and #task:1702's user-profile consumer for the human axis —
 * so both axes share one enumeration and neither pays for the other's caches.
 */
export function invalidateIdentityCarrierCaches(qc: QueryClient, kind: IdentitySubjectKind): void {
  for (const carrier of IDENTITY_CARRIERS) {
    if (!carrier.kinds.includes(kind)) continue;
    void qc.invalidateQueries(carrier.filter);
  }
}
