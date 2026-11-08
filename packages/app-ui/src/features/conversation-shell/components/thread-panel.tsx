import { usePageLive } from "@/navigation/page-stack";
import { useViewMemory } from "@/navigation/view-memory";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { paneChrome } from "@/components/layout/pane-chrome";
import {
  ArrowDownIcon,
  ArrowUpRightIcon,
  BellIcon,
  BellOffIcon,
  SearchIcon,
  ThreadIcon,
} from "@/icons";
import { authorGroupContinuations } from "@/features/messages/model/author-grouping";
import { groupMessages } from "@/features/messages/model/group-messages";
import { MessageItem } from "@/features/messages/components/message-item";
import { useAuthorAvatarObjectUrls } from "@/features/messages/hooks/author-avatar-hooks";
import { InlineFindBar } from "./inline-find-bar";
import { useInlineFindController, useInlineFindHotkey } from "@/features/search/use-inline-find";
import type { AdvancedSearchSeed } from "@/features/search/advanced-search-seed";
import type { SenderFilterValue } from "@/features/search/sender-filter-select";
import { SystemMessage } from "@/features/messages/components/system-message";
import { SystemActivityCluster } from "@/features/messages/components/system-activity-cluster";
import { Composer } from "@/features/messages/components/composer";
import { NativeComposerRoom } from "@/features/messages/components/native-composer-room";
import { SelectionQuoteToolbar } from "@/features/messages/components/selection-quote-toolbar";
import { ChannelJoinBar } from "@/features/messages/components/channel-join-bar";
import { MessageRowsSkeleton } from "@/features/conversation-shell/components/conversation-view-skeleton";
import { SidePanelHeader, SidePanelHeaderTitle } from "@/components/layout/side-panel-header";
import { iconMarkup, type PageBarDeclaration } from "@/components/layout/native-page-bar";
import { useMessageRenderContext } from "@/features/messages/hooks/use-message-render-context";
import { useMessageActionSurface } from "@/features/messages/hooks/use-message-action-surface";
import { MessageActionChrome } from "@/features/messages/components/message-action-chrome";
import {
  threadClusterShowsLoader,
  threadLiveMarkReadSeq,
  threadMarkReadSnapshot,
  threadOpenNeedsLatestRefetch,
  threadReplyRows,
  threadTailIsVisible,
  threadVisibleReplySeq,
} from "../model/thread-mark-read";
import type { WorkspaceChannelReference } from "@/features/messages/components/conversation-ref";
import {
  type JumpPhase,
  jumpWindowLockedForPhase,
  threadLocateAllowed,
} from "@/features/channels/message-jump-phase";
import type { MessageJumpTarget } from "@/features/channels/use-message-jump-controller";
import { TaskRefContextProvider } from "@/features/tasks/components/task-ref-context";
import {
  useFollowThread,
  useMarkThreadRead,
  useThreadCluster,
  useThreadFollowers,
  useThreadRoot,
  useUnfollowThread,
} from "@/features/channels/hooks";
import { isCommittedMessage, isPendingMessage } from "@/features/channels/pending-message";
import type { AttachmentSnapshot, MessageSeq } from "@silverhand/shared";
import type { ComposerDirectory } from "@/features/conversations/pane-mode";
import { useAgents } from "@/features/agents/hooks/hooks";
import { useAgentAvatarObjectUrls } from "@/features/agents/hooks/avatar-hooks";
import { useJoinChannel } from "@/features/workspaces/hooks";
import type { ChannelGate } from "@/features/channels/membership";
import { classifyTopShift } from "@/features/conversation-shell/model/message-list-window";
import { t, useLocale } from "@/lib/i18n";

interface ThreadPanelProps {
  /** Parent channel conversation. Replies live in this conversation with
   *  `thread_root_id = rootMessageId`; the panel reads/sends against it. */
  channelId: string;
  /** The parent conversation's workspace, or null for a `direct_pair` (no
   *  workspace coordinate). A pair's reply cluster resolves through the backend's
   *  `getChannelLike` just like a channel's — it spans every top-level placement,
   *  gated only by `canViewConversation` (#task:2096) — so this pane DOES mount for
   *  a pair. Null degrades only the workspace-scoped facets (`@`/`#` directory,
   *  channel-ref nav), exactly as the primary `ChannelView` handles its own
   *  nullable `conversation.workspace_id`. */
  workspaceId: string | null;
  /** The composer's candidate directory for this thread, projected by the pane
   *  owner (App) from the PARENT conversation via `inheritedComposerDirectory` — a
   *  thread has no placement of its own, so it inherits the parent's arm
   *  (#task:2147). Threaded down whole; never re-derived here from `workspaceId`,
   *  the route, or a lone boolean. A null parent (cold deep link before the parent
   *  resolves) arrives as the `withheld` arm, so the thread composer withholds both
   *  pickers rather than assuming a channel. */
  directory: ComposerDirectory;
  /** The root (main-line) message whose thread this panel shows. */
  rootMessageId: string;
  /** Parent channel title, shown in the header subtitle. */
  parentTitle?: string | null;
  /** Parent channel closed → composer disabled. */
  closed?: boolean;
  archived?: boolean;
  /** The parent conversation's affordance gate, resolved by App from the SAME
   *  authority `ChannelView` uses (`useChannelMembership`) and passed in
   *  ALREADY DERIVED — a thread shares its parent's gate, and re-deriving it
   *  here from a collapsed state is how the two panes would drift. For a
   *  channel: `member` participates fully; `nonmember` reads the thread with
   *  Join in place of the composer and every member affordance hidden;
   *  `unknown` (still resolving) is NEUTRAL — neither composer nor Join — so we
   *  never flash a Join we might revoke. For a `direct_pair` it is
   *  `directPairGate`, whose read-only arm withdraws replies without
   *  withdrawing mark-read. Required (no default) so a caller can't silently
   *  fail open. */
  gate: ChannelGate;
  onClose: () => void;
  onOpenAttachment: (attachment: AttachmentSnapshot) => void;
  /** "View in channel": close this panel and scroll-locate + flash the
   *  root message in the main channel stream. Wired by App to
   *  `requestJump` + `closeThread`. Omitted = button hidden. */
  onViewInChannel?: () => void;
  /** Inline-ref resolution + navigation, threaded from App so a thread
   *  reply resolves `#conv:` / `#<channel>` / `#task:` sigils identically
   *  to the channel pane (#task:1052). Same handlers App passes to
   *  `ChannelView`. */
  conversationRefsById?: Map<string, WorkspaceChannelReference>;
  onOpenConversationRef?: (id: string) => void;
  /** `(workspaceId, channelId)` — App-level handler; the panel pre-binds
   *  it to this thread's workspace before handing it to the renderer. */
  onOpenChannelRef?: (workspaceId: string, channelId: string) => void;
  /** `(taskId)` — App-level handler; wrapped into a
   *  `TaskRefContextProvider` scoped to this thread's parent channel so inline
   *  `#task:<n>` chips resolve against its board, then hand up the resolved id
   *  (which is scope-free) to open the detail modal. */
  onOpenTask?: (taskId: string) => void;
  /** A reply inside this cluster to scroll-locate + flash. Set by App when
   *  a jump (task/saved/search/notification) targets a message whose
   *  `thread_root_id === rootMessageId`. App gates this to this cluster, so
   *  the panel just locates. Null = no pending jump. */
  jumpMessageId?: string | null;
  /** The jump's `primeAround` lifecycle PHASE for a first-unread jump into
   *  THIS thread (#task:1317), or null when no jump is active. The single
   *  source of truth from which this pane projects three independent signals: the
   *  shared phase projections `jumpWindowLockedForPhase` (→ holds the
   *  cluster query disabled so a latest fetch can't clobber the around-window,
   *  through `committed` until `clearJump`) and `threadLocateAllowed` (→ poll only
   *  once `committed`/`settled`, never against a still-priming/stale window), plus
   *  the loader `threadClusterShowsLoader` (→ cover only while `priming`). */
  jumpPhase?: JumpPhase | null;
  /** Called once the jump completes (flashed) or is given up (target not in
   *  the loaded window). App clears the jump target on this. */
  onJumped?: () => void;
  /** In-conversation header find (#task:1335), scoped to THIS thread stream.
   *  When provided, the thread header shows a search-toggle icon. The callback
   *  navigates to a prev/next match via App's shared jump/prime path (carrying
   *  `threadRootId` so the cluster window is primed + this pane scroll-locates
   *  the reply). Absent → no find affordance (e.g. docked source viewer). */
  onJumpToMessage?: (params: MessageJumpTarget) => void;
  /** Opens the advanced `SearchPanel`, carrying the current inline query AND the
   *  narrowing this thread's PARENT conversation can express — its workspace, or
   *  the sender below for a `direct_pair` (shiki #task:1335, #task:2114). */
  onOpenAdvancedSearch?: (seed: AdvancedSearchSeed) => void;
  /** Sender seed for the hand-off above when this thread's PARENT is a
   *  `direct_pair` (#task:2114). Passed in rather than derived: a thread is not
   *  a conversation, so this panel holds no descriptor to project a pane mode
   *  from — its host does. Absent for a workspace channel's thread, which seeds
   *  `workspaceId` instead. */
  advancedSearchSender?: SenderFilterValue;
}

/**
 * Right-pane view of one thread: the root message plus its replies, read
 * from `GET /api/channels/:id/messages?thread_root_id=` (cut3-4) and
 * appended to via the channel send route with `reply_to_message_id`
 * (cut3-5). A thread is NOT a separate conversation — it is a reply cluster
 * sharing the parent channel's context — so this panel deliberately does
 * not reuse the conversation-bound `ChannelView`; it renders the thread
 * with the same `MessageItem` / `groupMessages` primitives and a
 * reply-targeted Composer.
 *
 * Render context (lookup maps + sigil resolution) comes from the shared
 * `useMessageRenderContext` hook — the SAME source `ChannelView`'s
 * `MessageList` consumes — so an inline `@mention` / `#<channel>` /
 * `#conv:` / `#task:` resolves identically in both panes (#task:1052).
 */
export function ThreadPanel({
  channelId,
  workspaceId,
  directory,
  rootMessageId,
  parentTitle,
  closed = false,
  archived = false,
  gate,
  onClose,
  onViewInChannel,
  onOpenAttachment,
  conversationRefsById,
  onOpenConversationRef,
  onOpenChannelRef,
  onOpenTask,
  jumpMessageId,
  jumpPhase = null,
  onJumped,
  onJumpToMessage,
  onOpenAdvancedSearch,
  advancedSearchSender,
}: ThreadPanelProps): JSX.Element {
  // `canParticipate` (member effects: follow / react / save / attachment export
  // + the mark-read effect below) rides the gate's member-effect axis; the
  // composer / Join slot uses the full gate so `unknown` shows NEITHER.
  const canParticipate = gate.allowMemberEffects;
  const rootCtx = useThreadRoot(channelId, rootMessageId);
  const cluster = useThreadCluster(channelId, rootMessageId, {
    jumpWindowLocked: jumpWindowLockedForPhase(jumpPhase),
  });
  const messages = cluster.data;
  // Window-position signals (mirrors the channel pane). A cluster can be
  // mid-history after an around-reply jump (saved/search/task/notification
  // → primeAround with threadRootId), so the back-to-bottom button must be
  // able to reset to the true tail, not just scroll the local window.
  const { isAtTail, hasNewerLiveGap, resetToLatest, windowRows, trimWindow } = cluster;
  const shouldResetToLatest = !isAtTail || hasNewerLiveGap;

  // Human follower state for this cluster (item 4 + 5). `viewer_following`
  // drives the header toggle / context-menu unfollow; `follower_agent_ids`
  // restricts the (channel-scoped) activity bubble to this thread's agents.
  // Thread follow is a member surface: a non-member's fetch is
  // disabled (null root), and the activity bubble then stays empty-allow-listed.
  const followers = useThreadFollowers(channelId, canParticipate ? rootMessageId : null);
  const viewerFollowing = followers.data?.viewer_following ?? false;
  // Until we have an authoritative follower snapshot, restrict the
  // (channel-scoped) activity bubble to an EMPTY allow-list so it shows
  // nothing — never fall back to unrestricted, which would leak main-line /
  // other-cluster agents into this thread. Showing full activity under
  // unknown state is wrong info, not a graceful fallback. `useThreadFollowers`
  // keeps this snapshot fresh against the thread message stream (auto-follow
  // side effects).
  const followerAgentIds = followers.data?.follower_agent_ids ?? [];
  const followThread = useFollowThread(channelId, rootMessageId);
  const unfollowThread = useUnfollowThread(channelId, rootMessageId);
  const markThreadRead = useMarkThreadRead();
  const live = usePageLive();
  const liveRef = useRef(live);
  liveRef.current = live;
  const followPending = followThread.isPending || unfollowThread.isPending;
  // Join the PARENT channel (the Composer-slot affordance for a non-member
  // browsing this public channel's thread).
  const joinChannel = useJoinChannel(workspaceId);
  const toggleFollow = (): void => {
    if (followPending) return;
    if (viewerFollowing) unfollowThread.mutate();
    else followThread.mutate();
  };

  // Shared render context — identical maps to the channel pane. The
  // AGENT avatar batch is lifted here (not in the hook) so this pane's
  // authors share one fetch; `useAgents` is React-Query cached so this
  // call reuses the same data the hook reads.
  const {
    agentsById,
    agentsByName,
    humansById,
    humansByName,
    channelsByName,
    viewerAvatarObjectUrl,
    viewer,
    userResolver,
    liveChannels,
    archivedChannels,
  } = useMessageRenderContext(workspaceId, channelId, canParticipate);
  const { data: agents = [] } = useAgents();
  const agentAvatarUrls = useAgentAvatarObjectUrls(agents);
  // Byline avatar bytes come STRICTLY from each rendered row's own author DTO
  // (#task:1792 B2) — the pinned root + every reply in this cluster — never the
  // workspace directory, so a deleted / departed author still renders its own
  // snapshot avatar or a neutral glyph. `agentAvatarUrls` (directory) stays the
  // source for the independent axis of inline `@handle` mention pills.
  const { agentAvatarUrls: authorAgentAvatarUrls, humanAvatarUrls: authorUserAvatarUrls } =
    useAuthorAvatarObjectUrls(messages.map((m) => (isCommittedMessage(m) ? m.author : null)));

  // ── Message actions (copy / share / multi-select export) ──────────────
  // Shared action surface — identical wiring to the channel pane
  // (#task:1067), so the per-row action props can't drift. The list/window
  // strategy is NOT shared: ThreadPanel stays a plain scroller / cluster
  // window. `selectionResetKey` is this pane's window-reset signal, bumped
  // on thread switch (rootMessageId effect below) and on any disjoint
  // cluster window replacement (detector below); the hook clears the active
  // selection when it changes.
  const [selectionResetKey, setSelectionResetKey] = useState(0);
  const actions = useMessageActionSurface({
    channelId,
    windowResetVersion: selectionResetKey,
    jumpToMessageId: jumpMessageId ?? null,
    canSave: canParticipate,
  });

  // Parent-channel TITLE for the export screenshot header + filename — the only
  // thing either consumer reads. The thread shares the parent channel's identity;
  // fall back to the `parentTitle` prop when the channel list hasn't resolved yet.
  // This used to also derive a `kind` from `roster_policy`, which nothing read
  // (#task:2077): a roster-derived identity word with no consumer is a face that
  // can go stale unobserved, and roster is the wrong key for identity anyway.
  const parentChannelTitle = useMemo(() => {
    const c =
      liveChannels.find((x) => x.id === channelId) ??
      archivedChannels.find((x) => x.id === channelId);
    return c ? c.title : (parentTitle ?? null);
  }, [liveChannels, archivedChannels, channelId, parentTitle]);

  // Drop any active selection when the cluster WINDOW is replaced (not just
  // grown by prepend/append or trimmed at the head). A disjoint replacement —
  // reset-to-latest, an off-tail composer send rebuilding to a tail page, a
  // reconnect invalidate, or an around-reply jump — leaves `selection.selected`
  // holding rows that are no longer in view. ChannelView gets this for free
  // from `useMessageSelection`'s `windowResetVersion`; the cluster needs the
  // same signal. We classify the top-of-window shift with the shared helper so
  // all three shapes are told apart:
  //   • prepend / append — old rows still present, selection stays valid.
  //   • head-trim — a live-appended latest page refetched to its fixed size
  //     drops HEAD rows (the old top vanishes), but the selection below the
  //     trimmed head survives, so it must NOT clear. The old "top-id gone =
  //     disjoint" test wrongly cleared here.
  //   • replace — genuinely disjoint window; the selection is stale → clear.
  // We deliberately do NOT update the ref across a transient empty window
  // (`resetToLatest` clears the cache to [] before the tail refetch lands), so
  // the pre-reset window is still remembered when the fresh window arrives.
  const prevKeysRef = useRef<string[]>([]);
  useEffect(() => {
    if (messages.length === 0) return; // keep prevKeysRef across transient empties
    const keys = messages.map((m) => m.id);
    const prevKeys = prevKeysRef.current;
    prevKeysRef.current = keys;
    if (classifyTopShift(prevKeys, keys).kind === "replace") {
      setSelectionResetKey((v) => v + 1);
    }
  }, [messages]);

  // Pre-bind the App handlers to this thread's workspace, mirroring
  // `ChannelView` exactly so the renderer receives the same shapes.
  const handleOpenChannelRef = useCallback(
    (resolvedChannelId: string) => {
      // A `#`-channel ref only resolves inside a workspace; a workspace-less pair
      // renders `#name` refs as inert text (no namespace), so there is nothing to
      // navigate to. Mirrors `ChannelView`'s own null-workspace guard.
      if (workspaceId === null) return;
      onOpenChannelRef?.(workspaceId, resolvedChannelId);
    },
    [onOpenChannelRef, workspaceId],
  );
  const taskRefContext = useMemo(
    // The thread's conversation for by-number resolution is its parent channel
    // — a thread shares the channel's board (its own conversation IS the
    // channel; the reply cluster is `thread_root_id`). `workspaceId` is the
    // cache-scope coord, so the thread and its channel share one by-number cell.
    () => ({ conversationId: channelId, workspaceId, onOpenTask }),
    [channelId, workspaceId, onOpenTask],
  );

  // In-conversation header find (#task:1335) is set up below, after the scroll
  // controller it depends on (`pinBottomRef` / `setShowScrollBottom`).

  // The root is pinned separately (see below), so drop it from the
  // scrolled reply list to avoid rendering it twice — it can appear in
  // the cluster window for small threads (where the window reaches the
  // start) but not for large ones (tail-anchored window). `threadReplyRows` is
  // the single authority for "which window rows are replies"; the read cursor
  // below derives from the same predicate.
  const replyMessages = useMemo(
    () => threadReplyRows(messages, rootMessageId),
    [messages, rootMessageId],
  );
  const items = useMemo(() => groupMessages(replyMessages), [replyMessages]);
  // Consecutive-same-author runs (#task:1737) — the same rule the channel
  // stream applies, over the same post-clustering sequence. Computed from
  // `items` (replies only), so the separately pinned root above the divider is
  // never in the set and the first reply always leads its own group.
  const authorContinuations = useMemo(() => authorGroupContinuations(items), [items]);

  // Authoritative full reply count from the cut3-6 sidecar (kept live by
  // useThreadRoot) — NOT a count of currently-loaded rows, which would be
  // wrong for a paginated long thread.
  const replyCount = rootCtx.replyCount;

  // Scroll-to-bottom behavior, mirroring the channel (#task:1050 items 4+5):
  // (re)opening the panel lands at the newest reply, and a back-to-bottom
  // button appears once the user scrolls up. ThreadPanel uses a plain
  // scroll container (not the channel's message list), so this is a small
  // vanilla controller rather than a reused hook.
  const viewMemory = useViewMemory();
  const readingKey = `thread:${channelId}:${rootMessageId}`;
  const savedReading = useMemo(() => viewMemory?.get(readingKey), [viewMemory, readingKey]);
  const restoredReading = useRef(false);
  const scrollRef = useRef<HTMLDivElement>(null);
  const pinBottomRef = useRef(true);
  const [showScrollBottom, setShowScrollBottom] = useState(false);

  const scrollToBottom = useCallback(() => {
    const el = scrollRef.current;
    if (!el) return;
    // Instant — same back-to-bottom command semantics as the channel.
    el.scrollTo({ top: el.scrollHeight, behavior: "auto" });
    pinBottomRef.current = true;
    setShowScrollBottom(false);
  }, []);

  // Back-to-bottom click: when the cluster window is mid-history (a forward
  // live gap, or an around-reply jump landed off-tail), local scrolling
  // can't reach the newest replies — reset the query to the tail window and
  // pin to bottom so the fresh data lands at the end. Otherwise just scroll
  // the loaded window to its end. (Mirrors ChannelView's bottom button.)
  const handleScrollBottomClick = useCallback(() => {
    if (shouldResetToLatest) {
      pinBottomRef.current = true;
      setShowScrollBottom(false);
      resetToLatest();
      return;
    }
    scrollToBottom();
  }, [shouldResetToLatest, resetToLatest, scrollToBottom]);

  // Mirror the tail-retreat action into a ref so the jump-locate effect can call
  // it on GIVE-UP (#task:1317 §4.3) without taking window-position values
  // (`shouldResetToLatest`) as effect deps — which would cancel/restart the poll
  // on every scroll. Retreating to the TRUE thread tail (reset the window if
  // mid-history, else scroll to bottom) is exactly `handleScrollBottomClick`, so
  // the "jump to latest" button and the locate give-up degrade identically.
  const retreatToTailRef = useRef(handleScrollBottomClick);
  retreatToTailRef.current = handleScrollBottomClick;

  const onScroll = useCallback(() => {
    const el = scrollRef.current;
    if (!el) return;
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight <= 20;
    viewMemory?.set(readingKey, { top: el.scrollTop, atBottom });
    pinBottomRef.current = atBottom;
    setShowScrollBottom(!atBottom);
  }, [viewMemory, readingKey]);

  // In-conversation header find (#task:1335), scoped to THIS thread stream
  // (stream='thread' + thread_root_id). Navigation is "ensure the match is
  // visible", NOT "reopen a window" (Cindy). This pane is a plain (non-
  // virtualized) scroller, so every LOADED reply is mounted:
  //   • match in the loaded data (a reply in `messages`, or the pinned root) →
  //     scroll to its already-mounted node locally — no prime, no cluster
  //     re-window, no white flash. We decide on the loaded DATA, not DOM
  //     presence, to stay consistent with the channel pane.
  //   • match off the loaded window → App's jump/prime path (carrying
  //     `threadRootId`) primes the cluster window and this pane's rAF locate
  //     (below) scroll-flashes the reply once it lands.
  // Offered only when the pane supports being jumped into (`onJumpToMessage`).
  // `findRootRef` is declared before `inlineFindNavigate` so the local locate can
  // scope its node lookup to THIS pane (see below).
  const findRootRef = useRef<HTMLElement>(null);
  const findEnabled = !!onJumpToMessage;
  // In-thread search is a member surface — the allowlist restores
  // it only on join. Gate the toggle, the ⌘F hotkey, the bar, AND the query
  // source (via the controller's `enabled`) together, mirroring ChannelView.
  const findAvailable = findEnabled && canParticipate;
  const inlineFindNavigate = useCallback(
    (messageId: string) => {
      const inWindow = messageId === rootMessageId || messages.some((m) => m.id === messageId);
      if (inWindow) {
        // Scope the lookup to THIS pane's root: the thread root is rendered in
        // BOTH the channel mainline and here with the same `id="msg-<root>"`, so
        // a global `document.getElementById` would resolve to the channel's copy
        // and scroll the WRONG pane (Cindy). Message ids are URL-safe, so the
        // attribute selector needs no escaping — its job is the pane SCOPE (dodge
        // the duplicate global id), not char-escaping.
        const node = findRootRef.current?.querySelector(`[id="msg-${messageId}"]`);
        if (node) {
          // Hold position: stop follow-bottom from yanking away from the match
          // when the next reply arrives (mirrors the jump-locate effect).
          pinBottomRef.current = false;
          setShowScrollBottom(true);
          node.scrollIntoView({ block: "center", behavior: "smooth" });
          return;
        }
      }
      onJumpToMessage?.({
        workspaceId: workspaceId ?? undefined,
        channelId,
        threadRootId: rootMessageId,
        messageId,
      });
    },
    [messages, onJumpToMessage, workspaceId, channelId, rootMessageId],
  );
  const find = useInlineFindController({
    conversationId: channelId,
    stream: "thread",
    threadRootId: rootMessageId,
    onNavigate: inlineFindNavigate,
    enabled: findAvailable,
  });
  // ⌘F opens the thread find — but only when focus is inside this panel (the
  // channel handles the nowhere-focus default). See useInlineFindHotkey.
  useInlineFindHotkey({
    rootRef: findRootRef,
    enabled: findAvailable,
    isPrimary: false,
    onOpen: find.openFind,
  });

  // Reopening the panel for a (different) root resets to follow-bottom mode
  // and drops any in-flight selection (the frame the user was selecting in
  // is gone).
  useEffect(() => {
    pinBottomRef.current = true;
    setShowScrollBottom(false);
    setSelectionResetKey((v) => v + 1);
  }, [rootMessageId]);

  // Mark this reply-cluster read once per open (#task:1226 surface 4, §9/§9.10).
  // Opening a thread IS reading it (read-to-clear whole opened thread), so we
  // advance the viewer's per-thread `conversation_reads` cursor to the
  // OPEN-TIME stream head. The snapshot must come from THIS open's window fetch
  // — not the previous open's cache React Query serves synchronously on reopen,
  // and not a `setQueryData` update. `threadMarkReadSnapshot` enforces that via
  // `windowFetchSucceededAt >= openedAt`.
  //
  // `openedAt` / the once-guard / "was this a jump open" are captured at
  // RENDER time (the earliest point — before effects, before a fast localhost
  // `primeAround` could resolve), keyed by (channelId, rootMessageId). The
  // once-guard is therefore re-armed per open, never a permanent per-root lock,
  // so a stale cache can't lock it before the fresh fetch.
  // Human-attention only — writes the read cursor, never delivery/wake.
  const openKeyRef = useRef("");
  const openedAtRef = useRef(0);
  const markedForOpenRef = useRef(false);
  // Thread mark-read forward-only gates. `null` = "nothing marked / in flight
  // yet" — explicit absence, not a 0 sentinel (#task:1267 Axis-B: never collapse
  // "none" into value 0). Both hold a MessageSeq for THIS thread stream.
  const markInflightSeqRef = useRef<MessageSeq | null>(null);
  const lastMarkedSeqRef = useRef<MessageSeq | null>(null);
  const liveMarkTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  // The latest tail seq that PASSED the visible-live gate but has not been marked
  // yet — the debounce may still be pending. Kept in sync with the gate by the
  // visible-live effect (set to the gated seq, or null the moment the gate closes
  // — scrolled up / backgrounded). Read only by the close/switch flush below, so a
  // seen-but-not-yet-debounced reply still advances the cursor when the panel is
  // closed within the 300ms window, WITHOUT ever swallowing a tail the user
  // scrolled away from (that path already nulled this ref).
  const liveGatedSeqRef = useRef<MessageSeq | null>(null);
  const isJumpOpenRef = useRef(false);
  const openKey = `${channelId}::${rootMessageId}`;
  if (openKeyRef.current !== openKey) {
    openKeyRef.current = openKey;
    openedAtRef.current = Date.now();
    markedForOpenRef.current = false;
    markInflightSeqRef.current = null;
    lastMarkedSeqRef.current = null;
    // Whether THIS open began as a first-unread jump. Captured once at open so
    // a later `clearJump` (which nulls `jumpMessageId`) can't flip it and
    // trigger a latest refetch that clobbers the around-window mid-read.
    isJumpOpenRef.current = jumpMessageId != null;
  }

  // Issue a mark-read to `seq`: forward-only, once per seq. `lastMarkedSeqRef`
  // advances on mutation SUCCESS only — a pending/failed mark must not look
  // cleared locally while the server cursor hasn't moved. The in-flight guard
  // collapses re-fires while one is pending; on error the cursor is untouched so
  // the next trigger retries. Human-attention only — writes the viewer's read
  // cursor, never delivery/wake.
  const markThreadReadMutate = markThreadRead.mutate;
  const issueMarkRead = useCallback(
    (seq: MessageSeq, onMarked?: () => void) => {
      // Mark-read is a member effect: a non-member browsing a
      // public channel's thread must never advance thread read-state. Behavior
      // gate, not a hidden control — the auto-effect itself is suppressed.
      if (!canParticipate) return;
      // Same-stream (this thread) MessageSeq compares, guarded on explicit
      // absence: a null gate ("nothing marked / none in flight") never blocks
      // the first mark (#task:1267 Axis-B).
      if (lastMarkedSeqRef.current !== null && seq <= lastMarkedSeqRef.current) return;
      if (markInflightSeqRef.current !== null && seq <= markInflightSeqRef.current) return;
      markInflightSeqRef.current = seq;
      markThreadReadMutate(
        { channelId, threadRootId: rootMessageId, snapshotSeq: seq },
        {
          onSuccess: () => {
            if (lastMarkedSeqRef.current === null || seq > lastMarkedSeqRef.current)
              lastMarkedSeqRef.current = seq;
            onMarked?.();
          },
          onSettled: () => {
            if (markInflightSeqRef.current === seq) markInflightSeqRef.current = null;
          },
        },
      );
    },
    [channelId, rootMessageId, markThreadReadMutate, canParticipate],
  );

  // §9.10: drive this open's window fetch explicitly (the query itself has
  // `refetchOnMount: false`). A NORMAL open forces a fresh latest window so its
  // mark snapshot is from this open and the gate opens. A JUMP open does NOT
  // refetch — `primeAround` already fetched the authoritative around-first-unread
  // window (and stamped the freshness epoch); a target-less latest refetch here
  // would clobber it. Keyed on (channelId, rootMessageId) so it runs once per
  // open, NOT on `jumpMessageId` (which clears after locate).
  useEffect(() => {
    if (threadOpenNeedsLatestRefetch({ isJumpOpen: isJumpOpenRef.current })) {
      void cluster.refetch();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [channelId, rootMessageId]);

  // OPEN-TIME mark (read-to-clear the whole opened thread): advance to the
  // open-time stream head, gated by `threadMarkReadSnapshot` to THIS open's
  // fresh window fetch. The once-guard `markedForOpenRef` is armed on mutation
  // SUCCESS only — NOT optimistically — so a failed open-time mark stays
  // retryable: `issueMarkRead` is forward-only + in-flight-guarded, so re-running
  // this effect while the mark is pending is a no-op, and on failure the guard
  // is never armed, so the next dep change (a new reply bumps `streamHeadSeq`, a
  // refetch bumps `windowFetchSucceededAt`) retries. The visible-live effect
  // below independently covers the pinned-tail case.
  useEffect(() => {
    if (markedForOpenRef.current || !live) return;
    const snapshot = threadMarkReadSnapshot({
      alreadyMarkedForOpen: markedForOpenRef.current,
      isFetching: cluster.isFetching,
      headSeq: cluster.streamHeadSeq,
      windowFetchSucceededAt: cluster.windowFetchSucceededAt,
      openedAt: openedAtRef.current,
    });
    if (snapshot === null) return;
    issueMarkRead(snapshot, () => {
      markedForOpenRef.current = true;
    });
  }, [
    channelId,
    rootMessageId,
    cluster.isFetching,
    cluster.streamHeadSeq,
    cluster.windowFetchSucceededAt,
    issueMarkRead,
    live,
  ]);

  // Document visibility as REACTIVE state. The live-mark gate reads visibility,
  // but `document.hidden` read inline isn't reactive — `visibilitychange` fires
  // no React render. Without this, a reply that arrives while the app is
  // backgrounded correctly stays unread, but when the user returns to the
  // foreground (tail reply now on screen) nothing re-runs the
  // gate, so it lingers unread until the next message/scroll/reopen. Mirroring
  // visibility into state makes the return-to-visible transition a dependency
  // that re-fires the live-mark effect.
  const [documentShown, setDocumentVisible] = useState(() => !document.hidden);
  const documentVisible = documentShown && live;
  useEffect(() => {
    const onVisibility = (): void => setDocumentVisible(!document.hidden);
    document.addEventListener("visibilitychange", onVisibility);
    return () => document.removeEventListener("visibilitychange", onVisibility);
  }, []);

  // VISIBLE-LIVE mark: a reply arriving while the panel is open + watched should
  // clear unread without a close/reopen. Follow the read cursor to the newest
  // reply ONLY while the committed DOM viewport reaches the true stream tail
  // AND the window is visible — never swallowing an unrendered tail (scrolled
  // up) or a backgrounded panel. The actual viewport is sampled here instead
  // of trusting `pinBottomRef`: that ref controls follow intent and can be stale
  // after a clamped/no-op jump scroll. `threadLiveMarkReadSeq` is the pure gate;
  // debounced so a
  // burst of replies collapses to one mark at the latest visible seq
  // (max-VISIBLE, not latest-known — `messages` is the loaded
  // window). `lastMarkedSeqRef` (advanced on success) keeps it forward-only.
  // `documentVisible` is a dep so returning to the foreground re-runs the gate.
  // The cursor comes from the REPLY rows only (`threadVisibleReplySeq`): the
  // cluster window also contains the pinned main-line root, whose seq belongs to
  // the channel stream and is beyond this thread's head — posting it would be a
  // `position_future` 400 on a reply-less thread (#task:1992). Null there, so
  // the gate holds and no request is sent.
  const maxLoadedReplySeq = threadVisibleReplySeq(messages, rootMessageId);
  useEffect(() => {
    const viewport = scrollRef.current;
    const seq = threadLiveMarkReadSeq({
      tailVisible:
        viewport !== null &&
        threadTailIsVisible({
          streamAtTail: isAtTail,
          scrollHeight: viewport.scrollHeight,
          scrollTop: viewport.scrollTop,
          clientHeight: viewport.clientHeight,
        }),
      documentVisible,
      maxVisibleReplySeq: maxLoadedReplySeq,
      lastMarkedSeq: lastMarkedSeqRef.current,
    });
    // Track the gated seq for the close/switch flush. When the gate closes
    // (scrolled up / backgrounded → seq null) this clears, so a later close can
    // never flush a tail the user is no longer watching (no-swallow preserved).
    liveGatedSeqRef.current = seq;
    if (seq === null) return;
    if (liveMarkTimerRef.current) clearTimeout(liveMarkTimerRef.current);
    liveMarkTimerRef.current = setTimeout(() => {
      liveMarkTimerRef.current = null;
      // Re-check at fire time: the user may have scrolled up or backgrounded
      // the app during the debounce window.
      const currentViewport = scrollRef.current;
      if (
        currentViewport === null ||
        !threadTailIsVisible({
          streamAtTail: isAtTail,
          scrollHeight: currentViewport.scrollHeight,
          scrollTop: currentViewport.scrollTop,
          clientHeight: currentViewport.clientHeight,
        }) ||
        document.hidden ||
        !liveRef.current
      ) {
        return;
      }
      issueMarkRead(seq);
    }, 300);
    return () => {
      if (liveMarkTimerRef.current) {
        clearTimeout(liveMarkTimerRef.current);
        liveMarkTimerRef.current = null;
      }
    };
  }, [showScrollBottom, documentVisible, maxLoadedReplySeq, isAtTail, issueMarkRead]);

  // Flush a SEEN-but-unmarked tail reply when THIS open ends — the panel closes
  // (unmount) or switches to another thread — before the 300ms visible-live
  // debounce fired. Without this, closing the panel within the debounce window
  // clears the pending timer and the read cursor stays behind the reply the user
  // already saw (the ghost-unread badge, #task:1519). Keyed on `issueMarkRead`,
  // which is itself keyed on (channelId, rootMessageId) — so the cleanup runs on
  // close (unmount) AND on switch-to-another-thread, but NOT on scroll/append.
  // Those change the gate inputs (a different effect) and already keep
  // `liveGatedSeqRef` truthful, so a tail the user scrolled away from is null here
  // and not flushed. `issueMarkRead` is forward-only + in-flight-guarded, so this
  // can only advance the cursor, never swallow the concurrent tail or double-mark.
  useEffect(() => {
    return () => {
      const seq = liveGatedSeqRef.current;
      liveGatedSeqRef.current = null;
      if (seq !== null) issueMarkRead(seq);
    };
  }, [issueMarkRead]);

  // Live replies grow the window past its page bound. Pinned to the tail (or
  // just sent), the oldest rows go and the pin below re-anchors the view;
  // otherwise the newest go, below the viewport, so the reader is not moved
  // and the window leaves the tail for the back-to-latest button.
  const overWindow = messages.length > windowRows;
  const lastMessage = messages[messages.length - 1];
  const lastIsOwnSend = lastMessage !== undefined && isPendingMessage(lastMessage);
  useLayoutEffect(() => {
    if (!overWindow || jumpMessageId || cluster.isFetchingPreviousPage) return;
    trimWindow(pinBottomRef.current || lastIsOwnSend ? "latest" : "earliest");
  }, [
    overWindow,
    messages,
    jumpMessageId,
    cluster.isFetchingPreviousPage,
    lastIsOwnSend,
    trimWindow,
  ]);

  // While pinned (initial open, or the user sitting at the tail), keep the
  // newest reply in view as the root loads / replies arrive. Instant jump
  // (not smooth) so an open never animates up from the top.
  useLayoutEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    if (!restoredReading.current && !cluster.isLoading) {
      restoredReading.current = true;
      if (savedReading && !savedReading.atBottom) {
        el.scrollTop = savedReading.top ?? 0;
        pinBottomRef.current = false;
        setShowScrollBottom(true);
      }
    }
    if (!pinBottomRef.current) return;
    el.scrollTop = el.scrollHeight;
    setShowScrollBottom(false);
  }, [messages, rootMessageId, cluster.isLoading, rootCtx.root, savedReading]);

  // Jump-locate: when App routes a jump to a reply inside THIS cluster,
  // scroll it into view + one-shot flash, mirroring the channel's
  // `MessageList` flash sequence. App gates `jumpMessageId` to messages
  // whose `thread_root_id === rootMessageId`, so the panel just locates.
  // A pending jump overrides follow-bottom so the locate scroll wins.
  // This plain container renders
  // its loaded rows synchronously, so a short rAF poll covers the window's
  // commit. A target outside the loaded window (an older paginated reply)
  // releases the jump rather than latching — cross-page locate is a
  // follow-up, not silently broken.
  useEffect(() => {
    if (!jumpMessageId) return;
    // Locate only once the around-window is the authoritative one for THIS open:
    // `committed` (window written to cache AND still locked, so it can't be
    // clobbered) or `settled` (best-effort fallback against whatever loaded).
    // NEVER while `priming` — polling a still-fetching / previous-open stale
    // window is the original mis-anchor bug (#task:1317). The effect re-runs
    // when the phase advances (dep below), so a committed prime kicks it off.
    if (!threadLocateAllowed(jumpPhase)) return;
    pinBottomRef.current = false;
    setShowScrollBottom(true);
    let raf = 0;
    let attempts = 0;
    let flashTimer: ReturnType<typeof setTimeout> | undefined;
    const tryLocate = (): void => {
      const node = findRootRef.current?.querySelector(`[id="msg-${jumpMessageId}"]`);
      if (node) {
        node.scrollIntoView({ block: "center" });
        node.classList.add("msg-jump-flash");
        flashTimer = setTimeout(() => {
          node.classList.remove("msg-jump-flash");
          onJumped?.();
        }, 3000);
        return;
      }
      attempts += 1;
      if (attempts > 60) {
        // Give up: the target isn't in the loaded window (a `settled` fallback
        // whose target is off-window, or an unexpected miss). Degrade to the
        // TRUE thread tail — reset the window if it's mid-history, else scroll
        // the loaded window to its bottom — never latch a half-way stale scroll
        // and never leave a stuck loader (#task:1317 §4.3). Then release the
        // jump so the phase clears (no "neither render nor release" state).
        retreatToTailRef.current();
        onJumped?.();
        return;
      }
      raf = requestAnimationFrame(tryLocate);
    };
    raf = requestAnimationFrame(tryLocate);
    return () => {
      cancelAnimationFrame(raf);
      if (flashTimer) clearTimeout(flashTimer);
    };
  }, [jumpMessageId, jumpPhase, onJumped]);

  const renderMessage = (m: (typeof messages)[number]): JSX.Element =>
    isCommittedMessage(m) && m.from_kind === "system" ? (
      <SystemMessage
        key={m.id}
        message={m}
        channelsByName={channelsByName}
        onOpenChannelRef={handleOpenChannelRef}
        conversationRefsById={conversationRefsById}
        onOpenConversationRef={onOpenConversationRef}
        humansByName={humansByName}
      />
    ) : (
      <MessageItem
        key={m.id}
        message={m}
        conversationId={channelId}
        onOpenAttachment={onOpenAttachment}
        canReact={!closed && !archived && canParticipate}
        canParticipate={canParticipate}
        canMutateLinkedTask={!closed && !archived && gate.allowMutations}
        agentsById={agentsById}
        agentsByName={agentsByName}
        humansByName={humansByName}
        humansById={humansById}
        channelsByName={channelsByName}
        onOpenChannelRef={handleOpenChannelRef}
        conversationRefsById={conversationRefsById}
        onOpenConversationRef={onOpenConversationRef}
        viewerAvatarObjectUrl={viewerAvatarObjectUrl}
        viewer={viewer}
        authorAgentAvatarUrls={authorAgentAvatarUrls}
        authorUserAvatarUrls={authorUserAvatarUrls}
        agentAvatarUrls={agentAvatarUrls}
        userResolver={userResolver}
        searchQuery={find.highlightQuery}
        searchActive={find.activeMessageId != null && m.id === find.activeMessageId}
        continuesPrevious={authorContinuations.has(m.id)}
        {...actions.rowActionProps(m)}
      />
    );

  const following = followers.data && canParticipate ? viewerFollowing : null;
  const locale = useLocale();
  const nativeBar = useMemo<PageBarDeclaration>(() => {
    const actions = [
      findAvailable
        ? {
            id: "find",
            label: t("conversationShell.searchThisThread"),
            icon: iconMarkup(SearchIcon),
            selected: find.open,
          }
        : null,
      following !== null
        ? {
            id: "follow",
            label: following
              ? t("conversationShell.unfollowThread")
              : t("conversationShell.followThread"),
            icon: iconMarkup(following ? BellOffIcon : BellIcon),
            selected: following,
          }
        : null,
      onViewInChannel
        ? {
            id: "view",
            label: t("conversationShell.viewInChannel2"),
            icon: iconMarkup(ArrowUpRightIcon),
          }
        : null,
    ].filter((action) => action !== null);

    return {
      title: parentTitle || t("conversationShell.thread"),
      subtitle: t("conversationShell.threadReplies", { count: replyCount }),
      titleAction: null,
      actions: actions.length > 0 ? [actions] : [],
      tabs: null,
      addTabs: null,
    };
  }, [locale, findAvailable, find.open, following, onViewInChannel, parentTitle, replyCount]);

  return (
    <TaskRefContextProvider value={taskRefContext}>
      <aside
        ref={findRootRef}
        data-panel="side"
        data-pane="thread"
        className="relative flex h-full min-h-0 w-full flex-col overflow-hidden"
        style={paneChrome}
      >
        <SidePanelHeader
          onClose={onClose}
          nativeBar={nativeBar}
          nativeHandlers={{
            action: ({ id }) => {
              if (id === "find") {
                if (find.open) {
                  find.close();
                } else {
                  find.openFind();
                }
              } else if (id === "follow") {
                toggleFollow();
              } else if (id === "view") {
                onViewInChannel?.();
              }
            },
          }}
          actions={
            <>
              {findAvailable ? (
                <button
                  type="button"
                  data-sticker
                  data-sticker-size="md"
                  data-sticker-tone="neutral"
                  onClick={find.open ? find.close : find.openFind}
                  aria-label={t("conversationShell.searchThisThread")}
                  aria-pressed={find.open}
                  title={t("conversationShell.searchThisThread")}
                  className={
                    "flex size-7 shrink-0 items-center justify-center rounded-(--control-radius) " +
                    (find.open
                      ? "bg-bg-hover text-text-primary"
                      : "text-text-secondary hover:bg-bg-hover hover:text-text-primary")
                  }
                >
                  <SearchIcon className="size-3.5" />
                </button>
              ) : null}
              {followers.data && canParticipate ? (
                <button
                  type="button"
                  data-sticker
                  data-sticker-size="md"
                  data-sticker-tone="neutral"
                  aria-label={
                    viewerFollowing
                      ? t("conversationShell.unfollowThread")
                      : t("conversationShell.followThread")
                  }
                  aria-pressed={viewerFollowing}
                  title={
                    viewerFollowing
                      ? t("conversationShell.unfollowThread")
                      : t("conversationShell.followThread")
                  }
                  onClick={toggleFollow}
                  disabled={followPending}
                  className={
                    "flex size-7 shrink-0 items-center justify-center rounded-(--control-radius) hover:bg-bg-hover disabled:opacity-50 " +
                    (viewerFollowing
                      ? "text-text-primary hover:text-text-primary"
                      : "text-text-secondary hover:text-text-primary")
                  }
                >
                  {viewerFollowing ? (
                    <BellOffIcon className="size-3.5" />
                  ) : (
                    <BellIcon className="size-3.5" />
                  )}
                </button>
              ) : null}
              {onViewInChannel ? (
                <button
                  type="button"
                  data-sticker
                  data-sticker-size="md"
                  data-sticker-tone="neutral"
                  aria-label={t("conversationShell.viewInChannel2")}
                  title={t("conversationShell.viewInChannel2")}
                  onClick={onViewInChannel}
                  className="flex size-7 shrink-0 items-center justify-center rounded-(--control-radius) text-text-secondary hover:bg-bg-hover hover:text-text-primary"
                >
                  <ArrowUpRightIcon className="size-3.5" />
                </button>
              ) : null}
            </>
          }
        >
          <SidePanelHeaderTitle
            icon={<ThreadIcon className="size-3.5 shrink-0 text-text-muted" />}
            title={parentTitle || t("conversationShell.thread")}
            subtitle={t("conversationShell.threadReplies", { count: replyCount })}
          />
        </SidePanelHeader>

        {/* Inline find bar (#task:1335) — a header OVERLAY, not a flex sibling of
          the reply scroller. Anchored just below the h-12 header (`top-12`) and
          out of flow (`absolute`) so opening / closing / typing never changes
          the scroller's height (shiki: 搜索框不应挤压对话流、不应触发重渲染).
          It drives only the search controller + overlay; the cluster window is
          untouched until the user navigates to a match (ensure-visible above). */}
        {findAvailable && find.open ? (
          <div data-thread-find className="absolute inset-x-0 top-12 z-(--z-sticky)">
            <InlineFindBar
              controller={find}
              onOpenAdvanced={() =>
                onOpenAdvancedSearch?.({
                  query: find.query,
                  workspaceId: workspaceId ?? undefined,
                  sender: advancedSearchSender,
                })
              }
              placeholder={t("conversationShell.searchThisThread")}
            />
          </div>
        ) : null}

        {rootCtx.isError ? (
          // Root unavailable (404 / legacy thread target / fetch failure).
          // Distinct from "no replies yet" — the thread can't be shown at
          // all, and replying would have no valid target.
          <div className="flex flex-1 items-center justify-center px-4 text-center text-xs text-text-muted">
            {t("conversationShell.thisMessageCantBe")}
          </div>
        ) : rootCtx.isLoading && !rootCtx.root ? (
          // Root still loading: the shared message skeleton under the thread
          // header, matching every other conversation-loading surface.
          <MessageRowsSkeleton />
        ) : (
          <div className="relative flex min-h-0 flex-1 flex-col">
            <MessageActionChrome
              surface={actions}
              renderCtx={{
                agentsByName,
                humansByName,
                channelsByName,
                conversationRefsById,
                agentAvatarUrls,
                viewer,
                viewerAvatarObjectUrl,
              }}
              workspaceId={workspaceId}
              conversationId={channelId}
              selectionKey={`${channelId}#cluster#${rootMessageId}`}
              channelTitle={parentChannelTitle}
            />
            <div
              ref={scrollRef}
              onScroll={onScroll}
              className="scrollbar-hide flex min-h-0 flex-1 flex-col overflow-y-auto px-(--message-list-gutter) py-2"
            >
              {/* Load earlier replies (between the root and the tail window). */}
              {cluster.hasPreviousPage ? (
                <button
                  type="button"
                  disabled={cluster.isFetchingPreviousPage}
                  onClick={() => void cluster.fetchPreviousPage()}
                  className="mx-auto mb-1 rounded px-2 py-1 text-xs text-text-muted hover:text-text-primary disabled:opacity-50"
                >
                  {cluster.isFetchingPreviousPage
                    ? t("conversationShell.loading")
                    : t("conversationShell.loadEarlierReplies")}
                </button>
              ) : null}

              {/* Root message — the cluster anchor, rendered inline as the
                first message in the stream. No longer pinned (#task:1050
                item 1) so replies get the panel's full height. */}
              {rootCtx.root ? renderMessage(rootCtx.root) : null}

              {/* Root's bottom divider — sets the thread's originating message
                (its subject) apart from the replies below it (#task:1735). It is
                the SAME kind of element as the panel header's bottom rule (an
                internal separator between stacked regions of one surface), so it
                speaks the same language: `border-border-subtle`, the 1px neutral
                hairline, in every theme — matching `SidePanelHeader`. poster's
                2px poster ink is reserved for FRAME borders (things enclosed on
                all sides + hard shadow); using it here read as a frame fragment
                next to the header hairline (#task:1735 shiki feedback). Full-bleed
                horizontally (its negative margin cancels the scroll container's gutter) so it
                runs edge to edge. `shrink-0` keeps it a hairline in the flex
                column even when the "No replies yet" empty state claims the
                remaining height. Gated on the root so it reads as the root's own
                bottom edge — present whether or not there are replies yet. */}
              {rootCtx.root ? (
                <div className="-mx-(--message-list-gutter) my-2 shrink-0 border-t border-border-subtle" />
              ) : null}

              {threadClusterShowsLoader({
                isLoading: cluster.isLoading,
                jumpPhase,
              }) ? (
                // Loader only while there is genuinely nothing THIS open can render
                // yet: a real in-flight fetch, OR a jump still `priming` (its
                // around-window has not committed). Once the prime advances to
                // `committed` the (still-locked) query's cache carries the
                // around-window and we render it even though the jump isn't done —
                // the controller keeps the window LOCKED until the visual
                // locate/flash calls `onJumped → clearJump`, and hiding the
                // messages would starve that locate of its DOM node.
                // `threadClusterShowsLoader` is a pure projection of `jumpPhase`
                // (committed-fact, zero wall-clock) — no deadlock (#task:1317).
                <MessageRowsSkeleton />
              ) : replyMessages.length === 0 ? (
                <div className="flex flex-1 items-center justify-center px-4 text-center text-xs text-text-muted">
                  {t("conversationShell.noRepliesYet")}
                </div>
              ) : (
                items.map((item) => {
                  if (item.kind === "cluster") {
                    return (
                      <SystemActivityCluster
                        key={item.messages[0]?.id ?? "cluster"}
                        messages={item.messages}
                        channelsByName={channelsByName}
                        onOpenChannelRef={handleOpenChannelRef}
                        conversationRefsById={conversationRefsById}
                        onOpenConversationRef={onOpenConversationRef}
                        humansByName={humansByName}
                      />
                    );
                  }
                  return renderMessage(item.message);
                })
              )}
              <div aria-hidden className="shrink-0" data-composer-activity-room />
              <NativeComposerRoom />
            </div>

            {/* Back-to-bottom — same primitive as the channel (#task:1050
              item 5). Appears once the user scrolls away from the tail. */}
            {showScrollBottom || shouldResetToLatest ? (
              <button
                type="button"
                data-scroll-bottom
                data-sticker
                data-sticker-size="lg"
                data-sticker-tone="neutral"
                aria-label={
                  shouldResetToLatest
                    ? t("conversationShell.jumpToLatestReply")
                    : t("conversationShell.scrollToBottom")
                }
                title={
                  shouldResetToLatest
                    ? t("conversationShell.jumpToLatestReply")
                    : t("conversationShell.scrollToBottom")
                }
                onClick={handleScrollBottomClick}
                className="absolute bottom-4 left-1/2 z-(--z-sticky) flex size-9 -translate-x-1/2 items-center justify-center rounded-(--pill-radius) bg-accent text-text-on-accent shadow-lg transition-all hover:opacity-90 active:opacity-75"
              >
                <ArrowDownIcon className="size-4" />
              </button>
            ) : null}
          </div>
        )}

        <SelectionQuoteToolbar
          boundaryRef={scrollRef}
          draftKey={`${channelId}#cluster#${rootMessageId}`}
        />

        {gate.showComposer ? (
          <Composer
            key={`${channelId}#cluster#${rootMessageId}`}
            conversationId={channelId}
            directory={directory}
            threadRootId={rootMessageId}
            disabled={closed || rootCtx.isError || !rootCtx.root}
            archived={archived}
            activityAgentIds={followerAgentIds}
          />
        ) : gate.showJoin ? (
          // Resolved non-member browsing a public channel's thread: Join (the
          // parent channel) replaces the reply composer. `unknown` (still
          // resolving) shows NEITHER — a neutral state — so we never flash a Join
          // affordance we might revoke.
          <ChannelJoinBar
            onJoin={() => joinChannel.mutate(channelId)}
            joining={joinChannel.isPending}
            channelTitle={parentChannelTitle}
          />
        ) : null}
      </aside>
    </TaskRefContextProvider>
  );
}
