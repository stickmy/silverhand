import type { MainTab } from "@/navigation/history";
import {
  movePane,
  closePane,
  editPanesUrl,
  openPane,
  ownedPane,
  paneKey,
  paneParent,
  panesUrl,
  parsePanes,
  type Pane,
  type Panes,
} from "@/navigation/panes";
import { useNavigationDriver, useNavigationReader, type Driver } from "@/navigation/provider";
import type { DirectMessageTarget } from "@/features/direct-pairs/target";
import { useCallback, useMemo } from "react";
import { matchPath, useLocation } from "react-router-dom";
import type { AppSection } from "@/components/layout/nav-rail";
import type { LocatorTarget } from "@/features/conversations/locator-target";

/**
 * Single source of truth for our URL scheme. Keep all path templates
 * here so callers don't sprinkle string concatenation across the
 * codebase.
 *
 * Naming: workspace (outer container) → channel (conversation). The base
 * path is `/chat/w/:workspaceId/c/:channelId`. There is no path-level
 * thread segment — the legacy isolated-thread `/t/<threadId>` fossil pane
 * was retired (the reply-cluster model replaces it). Everything beside the
 * route's conversation is the pane list in repeated `?pane=` params
 * (`navigation/panes.ts`); the base path is untouched by any pane.
 */
export const routes = {
  home: () => "/",
  chat: () => "/chat",
  /**
   * Advanced search, optionally SEEDED by the inline header find's hand-off
   * (#task:1335, extended #task:2114).
   *
   * The seed travels in the URL rather than being written straight into App's
   * session filter state, because a hand-off from a `direct_pair` has to say
   * two things at once — "filter to this counterpart" AND "forget the filters
   * you were carrying" — and a merge-into-state seed can only say the first
   * (a stale `workspace:` from an earlier search then filters the DM's own
   * results away, which is the reported bug).
   *
   * So the two things are TWO params: `reset=1` means "start from the filter
   * defaults", and each filter param names one axis to apply on top. They are
   * orthogonal on purpose — a hand-off sends `reset=1` even when it can express
   * NO axis (a pair whose counterpart is deleted seeds no sender, and a pair has
   * no workspace at all), which is the case an implicit "any filter param means
   * the URL owns the filter set" rule silently misses: nothing to send, so the
   * stale filters would survive the hand-off exactly as before.
   *
   * The sidebar's own Search entry passes no seed, so it keeps carrying the
   * session filters across pane switches (#task:1259), as does a bare `?q=`.
   */
  chatSearch: (seed?: { q?: string; workspace?: string; sender?: string; reset?: boolean }) => {
    const params = new URLSearchParams();
    const q = seed?.q?.trim();
    if (q) params.set("q", q);
    if (seed?.reset) params.set("reset", "1");
    if (seed?.workspace) params.set("workspace", seed.workspace);
    if (seed?.sender) params.set("sender", seed.sender);
    const qs = params.toString();
    return qs ? `/chat/search?${qs}` : "/chat/search";
  },
  chatSaved: () => "/chat/saved",
  chatArchived: () => "/chat/archived",
  chatWorkspace: (workspaceId: string) => `/chat/w/${workspaceId}`,
  chatChannel: (workspaceId: string, channelId: string) => `/chat/w/${workspaceId}/c/${channelId}`,
  /**
   * A WORKSPACE-LESS conversation (#task:2035): the `direct_pair` between the
   * viewer and one agent. One segment, because the conversation id is the WHOLE
   * coordinate — a pair has no container to name (`placement_kind =
   * 'direct_pair'` ⇒ `workspace_id IS NULL`), and `/chat/w/<something>/c/<id>`
   * could only be reached by inventing a workspace the row does not belong to.
   *
   * Deliberately a SECOND base path rather than making `:workspaceId` optional
   * on `chatChannel`: an optional segment would make "which container is this
   * pane in" a runtime question at every consumer, whereas two patterns let
   * `parseRouteState` answer it once (`directConversationId`) and let the
   * pane query writers — which operate on the current pathname, never on a
   * rebuilt one — carry over untouched.
   */
  chatDirect: (conversationId: string) => `/chat/d/${conversationId}`,
  /**
   * The direct line with one agent BEFORE it exists (#task:2039): the pane the
   * agent page's `Message` entry opens when the viewer has never messaged that
   * agent. It names the AGENT, because that is the only coordinate there is —
   * the pair is minted server-side by the first send, and until then nothing
   * has been allocated to name (opening and leaving this route persists
   * nothing, #task:2036's door contract).
   *
   * A fourth `new` segment under `/chat/d/` rather than a query flag: it is a
   * whole primary pane with its own content, exactly like the two arms beside
   * it, and `parseRouteState` can then answer "draft or pair" once. It cannot
   * collide with `chatDirect` — that pattern is three segments and this is
   * four.
   */
  chatDirectDraft: (target: DirectMessageTarget) =>
    target.kind === "agent" ? `/chat/d/new/${target.id}` : `/chat/d/new/human/${target.id}`,
  // Activity is a top-level NavRail section (peer to chat, #task:1394). Its
  // selected state is a pure projection of the route: bare `/activity` (no
  // workspace, no channel — the full-width global list) plus one selected-item
  // route PER PLACEMENT, because an Activity item is either a workspace channel
  // or a workspace-less DirectPair and BOTH open in the Activity shell/list
  // (opening a DM used to fall back to `/chat/d/:id` and leave the section — a
  // #task:2115 dogfood regression). There is deliberately NO workspace-only
  // `/activity/w/:wid` route: a workspace with no channel would be a selection
  // state with a titlebar/ws-scoped queries but no Activity item behind it
  // (Cindy, #task:1394 review). The DM route names a whole conversation, so it
  // does not reintroduce that empty-selection problem. Panes ride the shared
  // query helpers, which operate on the current pathname and thus stay
  // in-section automatically.
  activity: () => "/activity",
  activityChannel: (workspaceId: string, channelId: string) =>
    `/activity/w/${workspaceId}/c/${channelId}`,
  // The workspace-less DirectPair twin of `activityChannel` (#task:2115): a DM
  // selected in Activity, staying in the Activity shell. Disjoint from
  // `activityChannel` by its `/d/` segment (mirrors the `chatDirect` vs
  // `chatChannel` split), and from `chatDirect` by the `/activity` prefix.
  activityDirect: (conversationId: string) => `/activity/d/${conversationId}`,
  members: () => "/members",
  membersAgent: (agentId: string) => `/members/${agentId}`,
  // Distinct prefix for users so a user id can never collide with the
  // agent id namespace at the URL level. The AgentSidebarShell writes this
  // when a HUMANS row is clicked; AgentView and HumanView are then
  // mutually exclusive at the App level (parseRouteState produces one
  // of agentId / userId, never both).
  membersUser: (userId: string) => `/members/u/${userId}`,
  computers: () => "/computers",
  computer: (hostId: string) => `/computers?computer=${encodeURIComponent(hostId)}`,
  tasks: () => "/tasks",
  settings: () => "/settings",
  me: () => "/me",
} as const;

export const routePatterns = {
  chatChannel: "/chat/w/:workspaceId/c/:channelId",
  chatDirectDraft: "/chat/d/new/:agentId",
  chatHumanDirectDraft: "/chat/d/new/human/:userId",
  chatDirect: "/chat/d/:conversationId",
  chatWorkspace: "/chat/w/:workspaceId",
  chatArchived: "/chat/archived",
  chatSearch: "/chat/search",
  chatSaved: "/chat/saved",
  chat: "/chat",
  activityChannel: "/activity/w/:workspaceId/c/:channelId",
  activityDirect: "/activity/d/:conversationId",
  activity: "/activity",
  membersAgent: "/members/:agentId",
  membersUser: "/members/u/:userId",
  members: "/members",
  computers: "/computers",
  tasks: "/tasks",
  settings: "/settings",
  me: "/me",
} as const;

export type ChatView = "channel" | "saved" | "search" | "archived";

/**
 * Resolved view of the current URL. Components read this instead of
 * useState — single hook keeps the parsing in one place.
 */
export interface RouteState {
  section: AppSection;
  /** Within the chat section: 'channel' or 'saved'. */
  chatView: ChatView;
  /** Advanced-search prefill (`/chat/search?q=<query>`). Seeds the SearchPanel
   *  query on mount when the inline header find hands off to advanced search
   *  (#task:1335). Null when absent. */
  searchQuery: string | null;
  /**
   * Advanced-search FILTER seed (`?reset=1&workspace=<id>&sender=<user|agent>:<id>`,
   * #task:2114). The two axis params are raw strings: the sender vocabulary
   * belongs to the filter control, so validation happens where that union is
   * defined (`senderFilterFromParam`) rather than here — the router's job is only
   * "present and non-empty".
   *
   * `searchFilterReset` is what makes the seed a REPLACEMENT rather than a merge;
   * false ⇒ App leaves the session filters alone (see `routes.chatSearch`).
   */
  searchFilterReset: boolean;
  searchWorkspace: string | null;
  searchSender: string | null;
  workspaceId: string | null;
  channelId: string | null;
  /**
   * Set to `channelId` iff the URL is a workspace-less conversation route —
   * `/chat/d/:conversationId` (#task:2035) or its Activity twin
   * `/activity/d/:conversationId` (#task:2115) — the EXPLICIT discriminator for
   * "this pane has no workspace coordinate". It is section-independent: the
   * section is read from the pathname prefix; this only says the open
   * conversation is a pair.
   *
   * It exists because `workspaceId === null` does not mean that: a bare `/chat`,
   * `/chat/search`, and every non-chat section also carry a null workspace, and
   * App's auto-select effect reads exactly that condition to bounce the user into
   * their first workspace. Without a positive marker, landing on a pair would be
   * indistinguishable from landing nowhere and would be redirected away
   * immediately. `channelId` is populated as usual, so every consumer that only
   * needs "which conversation is open" (the panes, read cursors,
   * notification suppression) needs no new branch.
   */
  directConversationId: string | null;
  /**
   * Set to the agent id iff the URL is `/chat/d/new/:agentId` (#task:2039) — the
   * viewer is composing the FIRST message of a direct line that does not exist
   * yet.
   *
   * A sibling discriminator to `directConversationId`, never live at the same
   * time: this arm has no conversation at all, so `channelId` stays null and
   * every conversation-keyed consumer (read cursors, panes,
   * notification suppression) needs no new branch. What it DOES need to reach is
   * the workspace auto-restore effect, which reads "chat section + no workspace"
   * as a gap to fill and would otherwise navigate the draft away.
   */
  directDraftTarget: DirectMessageTarget | null;
  agentId: string | null;
  /** Set when the URL is `/members/u/:userId`. Mutually exclusive with
   *  `agentId` (the patterns are disjoint by segment count). The
   *  members section's right-pane branches on whichever is non-null. */
  userId: string | null;
  /** The pane list in display order. */
  panes: Panes;
}

/**
 * Pure URL → RouteState parser. Exported for unit testing — the React
 * hook below is just `parseRouteState(useLocation())`. Keeping this
 * separate avoids spinning up a router in tests.
 */
export function parseRouteState(pathname: string, search: string): RouteState {
  // Both chat and activity carry the same `/w/:workspaceId/c/:channelId`
  // shape (activity just under a different prefix), so try both channel
  // patterns — workspaceId/channelId then populate identically for either
  // section, and the section itself is decided by the pathname prefix below.
  const mChannel =
    matchPath(routePatterns.chatChannel, pathname) ??
    matchPath(routePatterns.activityChannel, pathname);
  // Workspace-only is CHAT-only: activity has no `/activity/w/:wid` route (a
  // workspace with no channel would be a third selection state with no Activity
  // item behind it — #task:1394). A hand-typed `/activity/w/foo` therefore
  // matches no pattern here and resolves to activity section + no workspace/no
  // channel (the full-width list), never a phantom workspace selection.
  const mWorkspace = matchPath(routePatterns.chatWorkspace, pathname);
  // The workspace-less conversation route. Disjoint from the two channel
  // patterns by its `/d/` segment, so no ordering question arises. It has TWO
  // prefixes — `/chat/d/:id` (#task:2035) and its Activity twin
  // `/activity/d/:id` (#task:2115) — and `directConversationId` is populated
  // the same way for either, because "this pane has no workspace coordinate" is
  // a placement fact independent of which section shell hosts it; the section
  // itself is decided by the pathname prefix below. Its pre-mint sibling
  // (`/chat/d/new/:agentId`, #task:2039) is tried FIRST: the two are already
  // disjoint by segment count (four vs three), and matching the more specific
  // one first keeps that intent visible rather than resting on a count the
  // reader has to verify. (There is no Activity draft twin — a first DM is
  // composed from a profile, never from the Activity list.)
  const mDirectDraft = matchPath(routePatterns.chatDirectDraft, pathname);
  const mHumanDirectDraft = matchPath(routePatterns.chatHumanDirectDraft, pathname);
  const mDirect =
    mDirectDraft || mHumanDirectDraft
      ? null
      : (matchPath(routePatterns.chatDirect, pathname) ??
        matchPath(routePatterns.activityDirect, pathname));
  // Order matters: the user route is more specific (`/members/u/:userId`,
  // three segments) than the agent route (`/members/:agentId`, two
  // segments), and matchPath returns the first match. Trying the user
  // pattern first means `/members/u/foo` resolves cleanly. The patterns
  // are still disjoint by segment count — `/members/:agentId` will not
  // match a three-segment URL — but checking explicitly keeps intent
  // visible to the next reader.
  const mUser = matchPath(routePatterns.membersUser, pathname);
  const mAgent = mUser ? null : matchPath(routePatterns.membersAgent, pathname);

  const workspaceId = mChannel?.params.workspaceId ?? mWorkspace?.params.workspaceId ?? null;
  // A pair's conversation id lands in `channelId` like any other open
  // conversation; `directConversationId` is what says it has no workspace.
  const directConversationId = mDirect?.params.conversationId ?? null;
  const directDraftTarget: DirectMessageTarget | null = mHumanDirectDraft?.params.userId
    ? { kind: "human", id: mHumanDirectDraft.params.userId }
    : mDirectDraft?.params.agentId
      ? { kind: "agent", id: mDirectDraft.params.agentId }
      : null;
  const channelId = mChannel?.params.channelId ?? directConversationId ?? null;
  const searchParams = new URLSearchParams(search);
  const panes = parsePanes(searchParams, channelId);

  // Advanced-search prefill query (`?q=`); empty treated as unset.
  const rawSearchQuery = searchParams.get("q");
  const searchQuery = rawSearchQuery && rawSearchQuery.length > 0 ? rawSearchQuery : null;
  // Advanced-search filter seed (#task:2114) — same non-empty-or-null rule.
  // `reset` is a presence flag: only the canonical `1` counts, so a hand-edited
  // `reset=false` reads as "no reset" instead of as a truthy string.
  const searchFilterReset = searchParams.get("reset") === "1";
  const rawSearchWorkspace = searchParams.get("workspace");
  const searchWorkspace =
    rawSearchWorkspace && rawSearchWorkspace.length > 0 ? rawSearchWorkspace : null;
  const rawSearchSender = searchParams.get("sender");
  const searchSender = rawSearchSender && rawSearchSender.length > 0 ? rawSearchSender : null;

  let section: AppSection = "chat";
  if (pathname.startsWith("/members")) section = "members";
  else if (pathname.startsWith("/computers")) section = "computers";
  else if (pathname.startsWith("/tasks")) section = "tasks";
  else if (pathname.startsWith("/settings")) section = "settings";
  else if (pathname.startsWith("/activity")) section = "activity";
  else if (pathname === "/me") section = "me";

  const chatView: ChatView = pathname.startsWith("/chat/saved")
    ? "saved"
    : pathname.startsWith("/chat/search")
      ? "search"
      : pathname.startsWith("/chat/archived")
        ? "archived"
        : "channel";

  return {
    section,
    chatView,
    searchQuery,
    searchFilterReset,
    searchWorkspace,
    searchSender,
    workspaceId: workspaceId ?? null,
    channelId: channelId ?? null,
    directConversationId,
    directDraftTarget,
    agentId: mAgent?.params.agentId ?? null,
    userId: mUser?.params.userId ?? null,
    panes,
  };
}

export function useRouteState(): RouteState {
  const { pathname, search } = useLocation();
  return useMemo(() => parseRouteState(pathname, search), [pathname, search]);
}

/**
 * The URL a locator chip navigates to (#task:2120) — the ONE place a resolved
 * {@link LocatorTarget} becomes a destination.
 *
 * THE ONLY NAVIGATION AUTHORITY FOR A LOCATOR (@Cindy, #task:2120 review point 2).
 * A locator arrives as a link node, so the browser's own href is a second, silent
 * route to somewhere — and that href is the SANITIZED spelling, not the one the
 * grammar accepted. The chip therefore takes the default action over and comes here
 * with the parser's output; nothing downstream reads the href.
 *
 * TOTAL over the target type, which is why it is a switch on `kind` with no
 * fallback: every arm of {@link LocatorTarget} is navigable by construction (the
 * resolver returns `null` for everything else), so a new arm has to answer this
 * question rather than inherit a guess.
 *
 * The thread leaf opens the Threads pane on both container routes, including the
 * workspace-less pair route — a pair's reply cluster resolves through the same
 * channel-like index and its pane mounts with a null workspace (#task:2096), so
 * `/chat/d/:id?pane=threads.<root>` is a real destination. Whether that root message
 * exists is the pane's answer, not this writer's.
 */
export function buildLocatorTargetUrl(target: LocatorTarget): string {
  switch (target.kind) {
    case "workspace":
      return routes.chatWorkspace(target.workspaceId);
    case "channel":
    case "direct_pair": {
      const base =
        target.kind === "channel"
          ? routes.chatChannel(target.workspaceId, target.conversationId)
          : routes.chatDirect(target.conversationId);
      return target.threadRootId
        ? paneUrl(base, [
            { kind: "threads", of: target.conversationId, thread: target.threadRootId },
          ])
        : base;
    }
  }
}

/** The conversation a URL path opens: the one its panes stand beside. */
function pathChannelId(pathname: string): string | null {
  return parseRouteState(pathname, "").channelId;
}

/** `pathname` with exactly `panes`. */
export function paneUrl(pathname: string, panes: Panes): string {
  return panesUrl(pathname, pathChannelId(pathname), panes);
}

/** The URL with its pane list rewritten through `edit`; other params stay. */
export function editPaneUrl(
  pathname: string,
  search: string,
  edit: (panes: Panes, routeId: string | null) => Panes,
): string {
  const routeId = pathChannelId(pathname);
  return editPanesUrl(pathname, search, routeId, (panes) => edit(panes, routeId));
}

/** The URL with the pane `key` names closed. */
export function closePaneUrl(pathname: string, search: string, key: string): string {
  return editPaneUrl(pathname, search, (panes) => closePane(panes, key));
}

/**
 * A channel URL that keeps the agent panel open across a channel switch: it
 * follows the new channel. Nothing else carries over.
 */
export function channelUrlKeeping(
  workspaceId: string,
  channelId: string,
  keep: { agent: string },
): string {
  return paneUrl(routes.chatChannel(workspaceId, channelId), [{ kind: "agent", id: keep.agent }]);
}

/**
 * Typed navigation helpers. All call sites that previously did
 * `setSelectedX` go through these — keeps URL construction in one
 * place and gives the call sites a self-documenting API.
 */
export type NavigationTarget =
  | { kind: "tab"; tab: MainTab }
  | { kind: "section"; tab: MainTab; root?: string }
  | { kind: "url"; url: string }
  | { kind: "conversation"; conversationId: string; workspaceId?: string }
  | {
      kind: "thread";
      rootMessageId: string;
      conversationId?: string;
      workspaceId?: string;
      /** The conversation whose Threads pane opens it; the route's by default. */
      of?: string;
    }
  | { kind: "conversation-settings"; conversationId: string }
  | { kind: "conversation-members"; conversationId: string; add?: boolean }
  | { kind: "attachments"; of?: string }
  | { kind: "attachment"; attachmentId: string; of?: string }
  | { kind: "threads"; of?: string }
  | { kind: "task"; taskId: string }
  | { kind: "agent"; agentId: string }
  | { kind: "human"; userId: string };

/** Business destinations resolve through the existing URL schema. The shell,
 * never the caller, decides whether a pane destination fills a page. */
export function resolveNavigationTarget(
  target: NavigationTarget,
  pathname: string,
  search: string,
): string {
  if (
    target.kind === "thread" &&
    target.conversationId &&
    parseRouteState(pathname, search).channelId !== target.conversationId
  ) {
    pathname = target.workspaceId
      ? routes.chatChannel(target.workspaceId, target.conversationId)
      : routes.chatDirect(target.conversationId);
    search = "";
  }
  const open = (pane: (routeId: string | null, panes: Panes) => Pane | null) =>
    editPaneUrl(pathname, search, (panes, routeId) => {
      const next = pane(routeId, panes);
      return next ? openPane(panes, next) : panes;
    });
  const of = (id: string | undefined) => (routeId: string | null, panes: Panes) =>
    paneConversation(panes, routeId, id);
  switch (target.kind) {
    case "tab":
    case "section":
      return `/${target.tab}`;
    case "url":
      return target.url;
    case "conversation":
      return target.workspaceId
        ? routes.chatChannel(target.workspaceId, target.conversationId)
        : routes.chatDirect(target.conversationId);
    case "thread":
      return open((routeId, panes) => {
        const id = of(target.of)(routeId, panes);
        return id ? { kind: "threads", of: id, thread: target.rootMessageId } : null;
      });
    case "threads":
      return open((routeId, panes) => {
        const id = of(target.of)(routeId, panes);
        return id ? { kind: "threads", of: id, thread: null } : null;
      });
    case "attachments":
      return open((routeId, panes) => {
        const id = of(target.of)(routeId, panes);
        return id ? { kind: "files", of: id, attachment: null } : null;
      });
    case "attachment":
      return open((routeId, panes) => {
        const id = of(target.of)(routeId, panes);
        return id ? { kind: "files", of: id, attachment: target.attachmentId } : null;
      });
    case "conversation-settings":
      return open(() => ({ kind: "settings", of: target.conversationId, page: null }));
    case "conversation-members":
      return open(() => ({
        kind: "settings",
        of: target.conversationId,
        page: target.add ? "add" : "members",
      }));
    case "task":
      return open(() => ({ kind: "task", id: target.taskId }));
    case "agent":
      return open(() => ({ kind: "agent", id: target.agentId }));
    case "human":
      return open(() => ({ kind: "human", id: target.userId }));
  }
}

/** The conversation a Threads or Files pane opens for: `of` when named,
 *  otherwise the one whose Threads or Files pane is last, else the route's. */
function paneConversation(
  panes: Panes,
  routeId: string | null,
  of: string | undefined,
): string | null {
  if (of !== undefined) return of;
  const owned = panes.findLast((p) => p.kind === "threads" || p.kind === "files");
  return owned && "of" in owned ? owned.of : routeId;
}

function navigateWith(
  driver: Driver,
  pathname: string,
  search: string,
  target: NavigationTarget,
  opts?: { replace?: boolean },
): void {
  if (target.kind === "tab") {
    driver.selectTab(target.tab);
    return;
  }
  if (target.kind === "section") {
    driver.openSection(target.tab, target.root);
    return;
  }
  const pane = target.kind !== "url" && target.kind !== "conversation";
  driver.go(resolveNavigationTarget(target, pathname, search), {
    replace: opts?.replace ?? pane,
    pane,
  });
}

/**
 * The navigation actions a message row needs, with identities that never
 * change: they read the location when they run instead of subscribing to it.
 * `useNav()` re-renders its consumer on every navigation, which across a
 * hundred rows is most of the cost of switching channels.
 */
export function useNavActions() {
  const read = useNavigationReader();
  return useMemo(() => {
    const navigate = (target: NavigationTarget) => {
      const { driver, location } = read();
      navigateWith(driver, location.pathname, location.search, target);
    };
    return {
      go: (url: string) => navigate({ kind: "url", url }),
      openAgentPanel: (agentId: string) => navigate({ kind: "agent", agentId }),
      openHumanPanel: (userId: string) => navigate({ kind: "human", userId }),
      /** A channel takes its workspace; a direct message has none. */
      openConversation: (workspaceId: string | null, conversationId: string) =>
        navigate(
          workspaceId
            ? { kind: "conversation", workspaceId, conversationId }
            : { kind: "conversation", conversationId },
        ),
    };
  }, [read]);
}

export function useNav() {
  const driver = useNavigationDriver();
  const { pathname, search } = useLocation();
  const { back, go: drive } = driver;
  const navigate = useCallback(
    (target: NavigationTarget, opts?: { replace?: boolean }) =>
      navigateWith(driver, pathname, search, target, opts),
    [driver, pathname, search],
  );
  const go = useCallback(
    (url: string, opts?: { replace?: boolean }) => navigate({ kind: "url", url }, opts),
    [navigate],
  );
  const edit = useCallback(
    (change: (panes: Panes, routeId: string | null) => Panes) =>
      drive(editPaneUrl(pathname, search, change), { replace: true }),
    [drive, pathname, search],
  );
  // Close the last pane `match` picks, or step it back to its list.
  const close = useCallback(
    (match: (pane: Pane) => boolean, back = false) =>
      edit((panes) => {
        const pane = panes.findLast(match);
        if (!pane) return panes;
        const parent = back ? paneParent(pane) : null;
        return parent ? openPane(panes, parent) : closePane(panes, paneKey(pane));
      }),
    [edit],
  );
  const openIn = (kind: "threads" | "files", of: string | undefined): boolean => {
    const routeId = pathChannelId(pathname);
    const panes = parsePanes(new URLSearchParams(search), routeId);
    const id = paneConversation(panes, routeId, of);
    return id !== null && ownedPane(panes, kind, id) !== null;
  };
  const ofKind =
    <K extends Pane["kind"]>(
      kind: K,
      test: (pane: Extract<Pane, { kind: K }>) => boolean = () => true,
    ) =>
    (pane: Pane) =>
      pane.kind === kind && test(pane as Extract<Pane, { kind: K }>);
  return useMemo(
    () => ({
      navigate,
      navigateBack: () => back(),
      go,
      toChat: () => navigate({ kind: "section", tab: "chat" }),
      toChatSearch: (seed?: Parameters<typeof routes.chatSearch>[0]) => go(routes.chatSearch(seed)),
      toChatSaved: () => go(routes.chatSaved()),
      toChatArchived: () => go(routes.chatArchived()),
      toActivity: () => navigate({ kind: "section", tab: "activity" }),
      toWorkspace: (id: string) => go(routes.chatWorkspace(id)),
      toChannel: (workspaceId: string, conversationId: string) =>
        navigate({ kind: "conversation", workspaceId, conversationId }),
      toDirect: (conversationId: string) => navigate({ kind: "conversation", conversationId }),
      toDirectDraft: (target: DirectMessageTarget) => go(routes.chatDirectDraft(target)),
      toMembers: (id?: string) => go(id ? routes.membersAgent(id) : routes.members()),
      toMembersUser: (id: string) => go(routes.membersUser(id)),
      toComputers: () => go(routes.computers()),
      toComputer: (hostId: string) => go(routes.computer(hostId)),
      toTasks: () => navigate({ kind: "section", tab: "tasks" }),
      toUtilitySection: (section: "members" | "computers" | "settings") =>
        navigate({ kind: "section", tab: "me", root: routes[section]() }),
      toSettings: () => go(routes.settings()),
      /** Close the pane with `key`. */
      closePane: (key: string) => edit((panes) => closePane(panes, key)),
      /** Move the pane with `key` to `index` in the row. */
      movePane: (key: string, index: number) => edit((panes) => movePane(panes, key, index)),
      /** Step the pane with `key` back one page, closing it from its first. */
      backPane: (key: string) => close((pane) => paneKey(pane) === key, true),
      openThread: (rootMessageId: string, of?: string) =>
        navigate({ kind: "thread", rootMessageId, of }),
      closeThread: () => close(ofKind("threads", (p) => p.thread !== null)),
      openThreadsList: () => navigate({ kind: "threads" }),
      closeThreadsList: () => close(ofKind("threads")),
      toggleThreadsList: (of?: string) =>
        openIn("threads", of)
          ? close(ofKind("threads", (p) => of === undefined || p.of === of))
          : navigate({ kind: "threads", of }),
      openFilesList: () => navigate({ kind: "attachments" }),
      closeFilesList: () => close(ofKind("files")),
      toggleFilesList: (of?: string) =>
        openIn("files", of)
          ? close(ofKind("files", (p) => of === undefined || p.of === of))
          : navigate({ kind: "attachments", of }),
      openAttachmentPanel: (attachmentId: string, of?: string) =>
        navigate({ kind: "attachment", attachmentId, of }),
      closeAttachmentPanel: () =>
        close(
          ofKind("files", (p) => p.attachment !== null),
          true,
        ),
      openChannelSettings: (conversationId: string) =>
        navigate({ kind: "conversation-settings", conversationId }),
      openChannelMembers: (conversationId: string, add = false) =>
        navigate({ kind: "conversation-members", conversationId, add }),
      closeChannelSettings: () => close(ofKind("settings")),
      openTask: (taskId: string) => navigate({ kind: "task", taskId }),
      closeTask: () => close(ofKind("task")),
      openAgentPanel: (agentId: string) => navigate({ kind: "agent", agentId }),
      closeAgentPanel: () => close(ofKind("agent")),
      openHumanPanel: (userId: string) => navigate({ kind: "human", userId }),
      closeHumanPanel: () => close(ofKind("human")),
    }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [navigate, go, edit, close, back, drive, pathname, search],
  );
}
