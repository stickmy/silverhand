import { useViewMemory } from "@/navigation/view-memory";
import { useCompactLayout } from "@/navigation/layout";
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useReducer,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { Virtuoso, type VirtuosoHandle } from "react-virtuoso";
import { ArrowDownIcon } from "@/icons";
import type {
  AttachmentSnapshot,
  MessageAuthorDto,
  ReplyBadgeSummary,
  StreamUnreadDto,
  ThreadReplyPreviewRow,
} from "@silverhand/shared";
import { ChannelMessageItem } from "@/features/messages/components/channel-message-item";
import { useMessageRenderContext } from "@/features/messages/hooks/use-message-render-context";
import { useAuthorAvatarObjectUrls } from "@/features/messages/hooks/author-avatar-hooks";
import { SystemMessage } from "@/features/messages/components/system-message";
import { SystemActivityCluster } from "@/features/messages/components/system-activity-cluster";
import { authorGroupContinuations } from "@/features/messages/model/author-grouping";
import { groupMessages } from "@/features/messages/model/group-messages";
import { useMessageActionSurface } from "@/features/messages/hooks/use-message-action-surface";
import { MessageActionChrome } from "@/features/messages/components/message-action-chrome";
import { NativeComposerRoom } from "@/features/messages/components/native-composer-room";
import { MessageRowsSkeleton } from "@/features/conversation-shell/components/conversation-view-skeleton";
import { EmptyConversation } from "./empty-conversation";
import { type CachedMessage, isCommittedMessage } from "@/features/channels/pending-message";
import type { WorkspaceChannelReference } from "@/features/messages/components/conversation-ref";
import {
  classifyTopShift,
  FIRST_ITEM_INDEX_START,
  getFollowOutputBehavior,
  getInitialTopMostItemIndex,
  getMessageListViewportBudget,
  INITIAL_BOTTOM_ANCHOR_STATE,
  reduceBottomAnchor,
  resolveTopShiftAction,
  shouldRemountForJump,
} from "@/features/conversation-shell/model/message-list-virtualization";
import { t } from "@/lib/i18n";

// ── Message list ────────────────────────────────────────────────────────

type ListContext = { leadingSlot?: ReactNode };
// Component identities must stay stable across loading and scroll state changes.
const listComponents = {
  // The spacer reserves the floating compact header's height inside the
  // scroller, so rows scroll beneath it. It is 0 on desktop and constant
  // while scrolling, so prepends keep their anchor.
  Header: ({ context }: { context?: ListContext }) => (
    <>
      <div aria-hidden data-pane-chrome-spacer />
      {context?.leadingSlot ? (
        <div className="mx-auto mb-1 max-w-300 px-(--message-list-gutter)">
          {context.leadingSlot}
        </div>
      ) : null}
    </>
  ),
  Footer: () => (
    <>
      <div className="h-3" data-composer-activity-room />
      <NativeComposerRoom />
    </>
  ),
};

export function MessageList({
  channelId,
  workspaceId,
  messages,
  isLoading,
  hasMoreBefore,
  isFetchingEarlier,
  onLoadEarlier,
  agentAvatarUrls,
  leadingSlot,
  highlightedMessageId,
  jumpToMessageId,
  jumpPrimePending = false,
  onJumped,
  isAtTail = true,
  hasNewerLiveGap = false,
  onResetToLatest,
  onOpenThread,
  replyCountsByRoot,
  replySummariesByRoot,
  threadUnreadByRoot,
  replyPreviewsByRoot,
  conversationRefsById,
  onOpenConversationRef,
  onOpenAttachment,
  onOpenChannelRef,
  canReact,
  canSave = true,
  canParticipate = true,
  canMutateLinkedTask = false,
  searchQuery,
  searchActiveMessageId,
  surfaceActive = true,
}: {
  channelId: string;
  /** The conversation's workspace, or `null` for a workspace-less one (a
   *  `direct_pair`, #task:2035) — forwarded to the render context, whose
   *  workspace-scoped lookups stay empty for it. */
  workspaceId: string | null;
  messages: CachedMessage[];
  isLoading: boolean;
  hasMoreBefore: boolean;
  isFetchingEarlier: boolean;
  onLoadEarlier: () => void;
  /** Map of agent id → avatar object URL. Lifted into ActiveChannelView
   *  so message authors, member chips, and the activity strip share a
   *  single batch fetch instead of three independent ones. */
  agentAvatarUrls: Map<string, string | null>;
  leadingSlot?: ReactNode;
  highlightedMessageId?: string | null;
  jumpToMessageId: string | null;
  /** When true, App is still resolving a `primeAround` for the current
   *  `jumpToMessageId`. The jump effect waits in the `idx === -1`
   *  branch instead of giving up. */
  jumpPrimePending?: boolean;
  onJumped?: () => void;
  /** True when the cached window reaches the conversation tail.
   *  Around-message jumps can leave the list in a valid mid-history
   *  window before any live message arrives; the center bottom button
   *  must reset to latest in that state, not scroll to the window end. */
  isAtTail?: boolean;
  /** True when the cache window doesn't include the conversation's
   *  tail and at least one live message has arrived since. This is a
   *  live-content hint only; `!isAtTail` is the durable window-state
   *  signal used for the button action. */
  hasNewerLiveGap?: boolean;
  onResetToLatest?: () => void;
  /** Opens a main-line message's thread (reply cluster) in the right
   *  pane. Receives the root message id. Undefined on secondary panes
   *  (no nested threads), which hides the reply affordance. */
  onOpenThread?: (rootMessageId: string, jumpToMessageId?: string) => void;
  /** Per-root reply counts from the message window's cut3-6 sidecar
   *  (`reply_counts_by_root`). Drives the inline "{count} Reply" badge.
   *  A root with 0 (or absent) shows the hover "Thread" action instead. */
  replyCountsByRoot?: Record<string, number>;
  /** Per-root enriched reply-badge summaries (P1, #task:1073). Optional —
   *  when a root has an entry the badge upgrades to avatars + last-reply
   *  time; absent → count-only fallback. */
  replySummariesByRoot?: Record<string, ReplyBadgeSummary>;
  /** Per-root per-viewer server-scored unread (#task:1226), from
   *  `useConversationUnread`. The SINGLE source of the chip's "N new" +
   *  first-unread — sparse (absent root ⇒ 0). Kept separate from the
   *  optimistically-patched `replySummariesByRoot` so the server-scored fact
   *  never lives in the message-window cache. */
  threadUnreadByRoot?: Record<string, StreamUnreadDto>;
  /** Per-root inline preview of the cluster's most recent replies (#task:1745),
   *  from the message window's `reply_previews_by_root` sidecar. Optional:
   *  absent root ⇒ no preview block (and the whole sidecar is absent until the
   *  backend ships it, which degrades to today's chip-only row).
   *
   *  Mainline only — the ThreadPanel renders the real replies. Read-inert: no
   *  cursor is advanced by rendering it (`docs/thread-unread-v1.md` §6). */
  replyPreviewsByRoot?: Record<string, ThreadReplyPreviewRow[]>;
  conversationRefsById?: Map<string, WorkspaceChannelReference>;
  onOpenConversationRef?: (id: string) => void;
  /** Open a `.md` / `.html` attachment in its previewer tab (#task:2122).
   *  Threaded to each row's attachment card; absent → md/html keep open-with-OS. */
  onOpenAttachment?: (attachment: AttachmentSnapshot) => void;
  /** Pre-bound (workspace already resolved) click handler for inline
   *  `#<channel_name>` chips. */
  onOpenChannelRef?: (channelId: string) => void;
  /** Whether the viewer may react in this channel — false when closed or
   *  archived. Threaded down to each MessageItem so the reaction write
   *  affordances hide in a read-only channel. */
  canReact: boolean;
  /** Whether save/unsave (收藏) is permitted. Defaults to `true`; `false` for a
   *  non-member browsing a public channel — the Save affordance
   *  hides. Copy / share (read-side export) stay available. */
  canSave?: boolean;
  /** Whether the viewer is a member who may perform member-only per-message
   *  actions — starting a new thread, following a thread, exporting an
   *  attachment (download / open-external). Defaults to `true`; `false` for a
   *  non-member browsing a public channel. Reading messages,
   *  opening EXISTING threads, and inline attachment PREVIEW stay available. It
   *  also gates the @human-mention roster fetch (`useConversationMembers`) —
   *  the roster is a member surface. */
  canParticipate?: boolean;
  /** Whether the viewer may write a linked task's status from its inline chip.
   *  The write axis (member of an open channel); defaults to `false`
   *  (fail-closed). Threaded down to each MessageItem so a non-member sees the
   *  chip read-only. */
  canMutateLinkedTask?: boolean;
  /** In-conversation header find (#task:1335). The active term — highlighted
   *  inside every rendered row's body via `MessageItem`→`Markdown`. */
  searchQuery?: string;
  /** The id of the CURRENT prev/next find match, framed + marked "current".
   *  Off-window matches are materialised by the shared jump/prime path (App's
   *  `handleJumpToMessage`) before they can be the active row. */
  searchActiveMessageId?: string | null;
  /** False when this list is mounted-but-covered by a front tab overlay
   *  (Tasks / attachment). Chat stays mounted so its scroll survives the
   *  round-trip, but its out-of-flow floating affordances (the
   *  scroll-to-bottom pill) sit at `--z-sticky`, above the `--z-raised`
   *  overlay, so they must not paint while another tab owns the surface.
   *  Default true: contexts without a tab strip (threads, activity) are
   *  always their own surface. */
  surfaceActive?: boolean;
}): JSX.Element {
  const viewMemory = useViewMemory();
  const compact = useCompactLayout();
  const viewportRef = useRef<HTMLDivElement>(null);
  const [viewportHeight, setViewportHeight] = useState(0);
  useLayoutEffect(() => {
    const node = viewportRef.current;
    if (!node) return;
    const measure = () => setViewportHeight(node.clientHeight);
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(node);
    return () => observer.disconnect();
  }, []);
  const viewportBudget = useMemo(
    () => getMessageListViewportBudget(compact, viewportHeight),
    [compact, viewportHeight],
  );
  const listContext = useMemo(() => ({ leadingSlot }), [leadingSlot]);
  const readingKey = `conversation:${channelId}`;
  const savedPosition = useMemo(() => viewMemory?.get(readingKey), [viewMemory, readingKey]);
  // Render context (lookup maps + sigil resolution + resolved viewer)
  // comes from the shared hook so the channel pane and the thread
  // reply-cluster pane resolve `@mention` / `#<channel>` / `#conv:` /
  // `#task:` identically (#task:1052). The AGENT avatar batch stays
  // lifted in `ActiveChannelView` (passed in as `agentAvatarUrls`) so
  // authors, the member strip, and the activity bubble share one fetch.
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
  const handleOpenThread = useCallback(
    (msg: CachedMessage, jumpToMessageId?: string): void => {
      // A pending optimistic row has no server id to anchor a thread on.
      if (!isCommittedMessage(msg)) return;
      onOpenThread?.(msg.id, jumpToMessageId);
    },
    [onOpenThread],
  );

  // Auto-scroll to bottom on new messages or when a new activity bubble
  // appears — except when a jump is pending (the jump effect below owns
  // scroll position in that case).
  //
  // We record whether the user was near the bottom (≤20px) *before* each
  // render using a scroll listener + ref. After render, `scrollHeight` has
  // already grown by the new content, so computing distFromBottom post-render
  // would mis-classify a user sitting at the old bottom as "not near bottom".
  // The ref is initialised to `true` so the very first render scrolls down.
  //
  // On channel switch, MessageList may not remount (React reuses the instance).
  // Reset to `true` whenever `channelId` changes so switching to a new channel
  // always starts in "follow bottom" mode regardless of the previous channel's
  // scroll position.
  //
  // Jump sets `jumpToMessageId`, which bypasses auto-scroll entirely while
  // the jump is in flight. The jump effect also explicitly sets the ref to
  // `false` right after `scrollIntoView`, so "resume follow" only happens
  // once the user manually scrolls back down — more deterministic than
  // waiting for the smooth-scroll animation to fire `onScroll`.
  //
  // ── Virtuoso integration ────────────────────────────────────────────────
  //
  // Migration from a hand-rolled `overflow-y-auto + messages.map()` scroller
  // to react-virtuoso. The data layer (useChannelMessages → MessageDto[])
  // and the grouping layer (groupMessages) are unchanged; only the visible
  // window is virtualized so DOM/layout/paint cost no longer scales with
  // total message count.
  //
  // Behaviour we keep parity on:
  //   - Stick-to-bottom on new live messages (`followOutput="auto"`)
  //   - Top-pull pagination when older history exists (`startReached`)
  //   - Scroll-to-bottom button visible when not at bottom
  //   - Jump-to-message (Saved / task refs) — implemented via
  //     `virtuosoRef.scrollToIndex` + a one-shot DOM flash class once the
  //     row lands in the live DOM (Virtuoso may not have it mounted at
  //     dispatch time).
  //   - Restore scroll anchor across prepend (firstItemIndex tracking)
  //
  // Behaviour we explicitly drop:
  //   - The synchronous `scrollTop = scrollHeight` flick on new messages —
  //     replaced by Virtuoso's auto-follow which is gated on at-bottom
  //     state (saner default and matches user intent).
  const items = useMemo(() => groupMessages(messages), [messages]);
  // Consecutive-same-author runs (#task:1737). Derived from the RENDERED
  // sequence (post-clustering) so an activity strip between two of my messages
  // correctly breaks the run. Same call in the ThreadPanel — one rule, both
  // surfaces.
  const authorContinuations = useMemo(() => authorGroupContinuations(items), [items]);

  // Byline + stacked-replier avatar bytes come STRICTLY from each rendered row's
  // own author DTO (#task:1792 B2) — committed messages' `author` plus every
  // reply-badge summary's `repliers` — never the workspace directory. So an
  // active author whose account has left the loaded directory still renders its
  // own snapshot avatar, and a deleted / unresolved author draws a neutral glyph
  // with no fetch. The directory batch (`agentAvatarUrls`, lifted in
  // ActiveChannelView) stays the source for the INDEPENDENT axis of inline
  // `@handle` mention pills inside each message body. Pending optimistic rows
  // have no server author; they are the viewer's own send and resolve their
  // byline from `viewerAvatarObjectUrl`, so they need not enter this batch.
  const bylineAuthors = useMemo(() => {
    const authors: (MessageAuthorDto | null)[] = [];
    for (const m of messages) {
      if (isCommittedMessage(m)) authors.push(m.author);
    }
    if (replySummariesByRoot) {
      for (const summary of Object.values(replySummariesByRoot)) {
        for (const r of summary.repliers) authors.push(r);
      }
    }
    return authors;
  }, [messages, replySummariesByRoot]);
  const { agentAvatarUrls: authorAgentAvatarUrls, humanAvatarUrls: authorUserAvatarUrls } =
    useAuthorAvatarObjectUrls(bylineAuthors);

  const virtuosoRef = useRef<VirtuosoHandle>(null);
  const [isAtBottom, setIsAtBottom] = useState(true);
  const [showScrollBottom, setShowScrollBottom] = useState(false);
  const [flashedMessageId, setFlashedMessageId] = useState<string | null>(null);
  // Last find match we locally scrolled to (see the in-window find effect
  // below). Guards against re-scrolling on every unrelated `items` change.
  const lastScrolledSearchIdRef = useRef<string | null>(null);

  // `firstItemIndex` shifts down as older messages prepend so the visible
  // logical index stays stable and Virtuoso can hold scroll position. We
  // start way above zero so we have headroom for many prepend rounds.
  const [firstItemIndex, setFirstItemIndex] = useState(FIRST_ITEM_INDEX_START);
  const [windowResetVersion, setWindowResetVersion] = useState(0);
  // Back-to-bottom re-anchor version — deliberately SEPARATE from
  // `windowResetVersion`. Both feed Virtuoso's `key` to force a remount,
  // but only `windowResetVersion` / `jumpToMessageId` flow into
  // `useMessageActionSurface` (below) as selection-clearing signals. A
  // back-to-bottom must re-anchor the list WITHOUT wiping the user's
  // message selection, so it bumps this instead. See the button onClick.
  const [bottomAnchorVersion, setBottomAnchorVersion] = useState(0);
  // Back-to-bottom anchor lifecycle (see `reduceBottomAnchor`). `intent` feeds
  // `initialTopMostItemIndex` + the window-replacement remount decision so the
  // next intended mount lands at the tail; the one-shot reverts it to
  // 'default' the moment that mount lands, so later unrelated window churn
  // does not re-drag the user down. A state machine (not a bare boolean)
  // because the tail and reset branches consume on different signals and the
  // reset's transient empty must be distinguished from the pre-reset window.
  // NOT part of the Virtuoso `key` — it only influences the mount anchor.
  const [bottomAnchor, dispatchBottomAnchor] = useReducer(
    reduceBottomAnchor,
    INITIAL_BOTTOM_ANCHOR_STATE,
  );
  // Shared message action surface — selection state + copy / share /
  // export handlers, identical to the thread reply-cluster pane
  // (#task:1067), so the per-row action props can't drift apart. The
  // list/window strategy is NOT shared: this pane keeps Virtuoso + the
  // main-stream window reset. `windowResetVersion` / `jumpToMessageId` are
  // the selection-clearing signals for a latest↔around window replacement.
  const actions = useMessageActionSurface({
    channelId,
    windowResetVersion,
    jumpToMessageId,
    canSave,
  });
  // Channel TITLE — surfaced in the screenshot header and used as the suggested
  // filename base, and the only field the consumer reads. Top-level channels live
  // in liveChannels; we only want the title, so an inline find is fine. It used to
  // also derive a `kind` from `roster_policy` that nothing read (#task:2077).
  const channelTitle = useMemo(() => {
    const c =
      liveChannels.find((x) => x.id === channelId) ??
      archivedChannels.find((x) => x.id === channelId);
    return c ? c.title : null;
  }, [liveChannels, archivedChannels, channelId]);
  const [previousWindow, setPreviousWindow] = useState({ channelId, items });
  const lastJumpRemountKeyRef = useRef<string | null>(null);

  const itemKey = useCallback((item: ReturnType<typeof groupMessages>[number]) => {
    return item.kind === "cluster" ? item.id : item.message.id;
  }, []);

  // Index of the current jump target inside `items`, shared by
  // window-reset decisions and Virtuoso's `initialTopMostItemIndex`.
  // Returns -1 while App is still priming the around-window.
  const jumpTargetIdx = useMemo(() => {
    if (!jumpToMessageId) return -1;
    return items.findIndex((it) => it.kind === "message" && it.message.id === jumpToMessageId);
  }, [items, jumpToMessageId]);

  // Reset Virtuoso bookkeeping whenever the channel changes — the user is
  // entering a fresh stream and the stale baselines from the previous
  // channel must not leak in.
  useEffect(() => {
    // Do NOT reset `bottomAnchorVersion` here: `channelId` is already in the
    // Virtuoso key, so the channel switch remounts on its own. Bumping this
    // version back to 0 would remount a SECOND time (extra cache discard /
    // flicker on the hot channel-switch path). It has no cross-channel
    // semantics — a monotonic counter is fine. Cancel any in-flight bottom
    // request instead (not in the key, so this does not remount).
    dispatchBottomAnchor({ type: "cancel" });
    setIsAtBottom(true);
    setShowScrollBottom(false);
    setFlashedMessageId(null);
  }, [channelId]);

  // New jump attempt boundary — fires whenever `jumpToMessageId` flips
  // to a new non-null value. Clear any local flash from the previous
  // attempt; positioning is handled by remounting Virtuoso below.
  //
  // MUST be a layout effect, not passive: the jump remount below (which
  // bumps `windowResetVersion`) is a layout effect too. If this reset ran
  // passively it would fire AFTER the remount render, so that render would
  // still see a `'bottom'` intent from a prior back-to-bottom, force
  // `preferBottom`, and land the fresh jump at the tail instead of centering
  // on the target. Running here cancels the bottom request in the same layout
  // phase, so the remount render sees the 'default' intent.
  useLayoutEffect(() => {
    if (!jumpToMessageId) {
      lastJumpRemountKeyRef.current = null;
      return;
    }
    // A fresh jump overrides any prior back-to-bottom request so the mount
    // centers on the target.
    dispatchBottomAnchor({ type: "cancel" });
    setFlashedMessageId(null);
  }, [jumpToMessageId, channelId]);

  // Keep `firstItemIndex` and the Virtuoso mount in sync with how the loaded
  // window's HEAD moved between renders. Three shapes, told apart by old/new
  // key overlap (`classifyTopShift`):
  //
  //   • prepend   — older rows loaded above the old top (fetchPreviousPage):
  //                 shift firstItemIndex DOWN so the visible index stays stable
  //                 and Virtuoso holds scroll position.
  //   • head-trim — rows dropped from the HEAD while the rest stays contiguous.
  //                 This is #task:1505: the latest cache page was live-appended
  //                 past the page limit, so a refetch re-fetches the fixed-size
  //                 latest window and trims the surplus head rows. The old top
  //                 vanishes but the reader's anchor (below the trim) survives —
  //                 shift firstItemIndex UP and do NOT remount, so the reader is
  //                 not yanked to the default bottom. A user positioning command
  //                 (jump / back-to-bottom) in flight falls through to
  //                 replacement semantics so it keeps owning the anchor.
  //   • replace   — a disjoint window (latest ↔ around-message jump): reset the
  //                 baseline and remount per `shouldRemountForWindowReplacement`.
  //                 Without the reset Virtuoso's size cache stays anchored to the
  //                 long-gone layout and any subsequent scrollToIndex misses.
  //
  // Reconcile during render, before Virtuoso receives either prop. A parent
  // layout effect is too late: the child already committed the new data with
  // the OLD index and measured those rows against the previous size cache.
  // React retries this component before rendering children when its own state
  // changes here, so data and its absolute index are published together.
  if (previousWindow.channelId !== channelId) {
    setPreviousWindow({ channelId, items });
    setFirstItemIndex(FIRST_ITEM_INDEX_START);
    setWindowResetVersion(0);
  } else if (previousWindow.items !== items) {
    setPreviousWindow({ channelId, items });
    const shift = classifyTopShift(previousWindow.items.map(itemKey), items.map(itemKey));
    if (shift.kind !== "none") {
      const action = resolveTopShiftAction(shift, {
        userPositioning: jumpToMessageId != null || bottomAnchor.intent === "bottom",
        hasJumpTarget: jumpToMessageId != null,
        jumpTargetIndex: jumpTargetIdx,
        preferBottom: bottomAnchor.intent === "bottom",
      });
      if (action.firstItemIndex === "reset") {
        setFirstItemIndex(FIRST_ITEM_INDEX_START);
        if (action.remount) setWindowResetVersion((v) => v + 1);
      } else if (action.delta !== 0) {
        setFirstItemIndex((index) => index + action.delta);
      }
    }
  }

  // Treat every resolved jump as a fresh positioning request. This keeps
  // saved-message jumps on the same channel on the same path as
  // cross-channel jumps: Virtuoso mounts with `initialTopMostItemIndex`
  // already pointing at the target, instead of reusing an old size cache
  // and issuing an imperative scroll that can land on the previous
  // around-window's local position.
  useLayoutEffect(() => {
    if (!jumpToMessageId) {
      lastJumpRemountKeyRef.current = null;
      return;
    }
    if (
      !shouldRemountForJump({
        hasJumpTarget: true,
        jumpTargetIndex: jumpTargetIdx,
      })
    ) {
      return;
    }
    const target = items[jumpTargetIdx];
    if (!target) return;
    const key = `${channelId}:${jumpToMessageId}:${itemKey(target)}`;
    if (lastJumpRemountKeyRef.current === key) return;
    lastJumpRemountKeyRef.current = key;
    setWindowResetVersion((v) => v + 1);
  }, [channelId, items, itemKey, jumpTargetIdx, jumpToMessageId]);

  // ── Back-to-bottom one-shot consumption ──────────────────────────────────
  //
  // The armed bottom intent must be spent by the mount it authorized, then
  // revert to 'default' so later, unrelated window churn (a refetch after the
  // user scrolled up to read history) doesn't drag them back to the tail.
  // The two branches land on DIFFERENT signals, so they consume separately
  // (see `reduceBottomAnchor`). Both dispatch AFTER commit, strictly later
  // than the render that read `bottomAnchor.intent` for the mount anchor.
  //
  // Tail branch: the `bottomAnchorVersion` remount reads the armed intent
  // during its render; this effect (its commit) then spends it. Bound to the
  // version generation, not `isAtBottom` — a non-tail local bottom lies.
  useLayoutEffect(() => {
    dispatchBottomAnchor({ type: "tail-shown" });
  }, [bottomAnchorVersion]);

  // Reset branch: `resetToLatest()`→`resetQueries` clears the cache to []
  // first, so Virtuoso unmounts (loading/empty) and re-mounts naturally when
  // the latest rows return — bumping no version. Consumption cannot key off a
  // version, and cannot fire on the pre-reset stale window (also non-empty at
  // click time). So track the actual rows-shown/hidden lifecycle: the reducer
  // latches the transient empty (`rows-hidden`) and consumes only on the
  // following real mount (`rows-shown`). `hasRenderedRows` mirrors exactly
  // when Virtuoso is on screen with content.
  const hasRenderedRows = !isLoading && items.length > 0;
  useLayoutEffect(() => {
    dispatchBottomAnchor({ type: hasRenderedRows ? "rows-shown" : "rows-hidden" });
  }, [hasRenderedRows]);

  // Bottom button visibility tracks `isAtBottom`. Virtuoso fires
  // `atBottomStateChange` only when the boolean flips, so this stays
  // stable across rapid scroll events.
  useEffect(() => {
    setShowScrollBottom(!isAtBottom);
  }, [isAtBottom]);

  const handleStartReached = useCallback(() => {
    if (!surfaceActive || jumpToMessageId || !hasMoreBefore || isFetchingEarlier) return;
    onLoadEarlier();
  }, [surfaceActive, jumpToMessageId, hasMoreBefore, isFetchingEarlier, onLoadEarlier]);

  // Shared side-effects of the back-to-bottom command: cancel the in-flight
  // jump and clear the local flash/button. The bottom INTENT itself is armed
  // via `dispatchBottomAnchor` in the button onClick (tail vs reset), so the
  // one-shot lifecycle owns it. Back-to-bottom is a cancel-jump boundary, so
  // tear the jump down on both sides: `onJumped()` clears the parent's jump
  // target, and clearing `flashedMessageId` cancels the local 3s flash timer +
  // highlight (its effect cleanup fires) so a user scrolling back up within
  // that window doesn't see the stale highlight of a jump they just cancelled.
  // followOutput can then stick to the tail again.
  const cancelJumpForBottom = useCallback(() => {
    if (jumpToMessageId) onJumped?.();
    setFlashedMessageId(null);
    setShowScrollBottom(false);
  }, [jumpToMessageId, onJumped]);
  const shouldResetToLatest = !isAtTail || hasNewerLiveGap;
  const savedIndex = savedPosition?.anchor
    ? items.findIndex((item) => itemKey(item) === savedPosition.anchor)
    : -1;
  const restoringPosition =
    !jumpToMessageId &&
    bottomAnchorVersion === 0 &&
    savedPosition &&
    !savedPosition.atBottom &&
    savedIndex >= 0;
  const initialTopMostItemIndex = restoringPosition
    ? savedIndex
    : getInitialTopMostItemIndex({
        jumpTargetIndex: jumpTargetIdx,
        hasJumpTarget: jumpToMessageId != null,
        itemCount: items.length,
        preferBottom: bottomAnchor.intent === "bottom",
      });

  // Jump-to-message completion gate. Actual positioning is declarative:
  // when the target exists, the layout effect above remounts Virtuoso and
  // `initialTopMostItemIndex` opens directly at that row. This effect only
  // decides whether to wait for prime, give up on a final miss, or start
  // the one-shot flash/onJumped sequence.
  //
  // `jumpPrimePending` lets us distinguish "target not in cache yet"
  // (App is priming an around-window — wait for the next render) from
  // "primed and not present" (App will have cleared `jumpTarget` on
  // 404; for any other miss, signal `onJumped` so the parent doesn't
  // sit on a stuck target).
  useEffect(() => {
    if (!jumpToMessageId) return;
    if (items.length === 0) {
      // Empty cache. If App is priming, wait; otherwise let the next
      // data update re-trigger this effect — clearing the target now
      // would lose the user's visual hint.
      return;
    }
    if (jumpTargetIdx === -1) {
      if (jumpPrimePending) return; // wait for prime
      onJumped?.();
      return;
    }
    if (isFetchingEarlier) return;

    setShowScrollBottom(true);
    setFlashedMessageId(jumpToMessageId);
  }, [jumpToMessageId, jumpPrimePending, jumpTargetIdx, items.length, isFetchingEarlier, onJumped]);

  // One-shot flash lifecycle. The flash *visual* is declarative — the row
  // renders `.msg-jump-flash` from its `jumpFlash` prop (see `renderItem`),
  // so whichever DOM node Virtuoso mounts for this id picks the animation
  // up. This is deliberately NOT an imperative `getElementById().classList`
  // toggle: a jump remounts Virtuoso (`shouldRemountForJump` fires even when
  // the target is already on-screen) and `markRead`'s refetch can reset the
  // window — both replace the target's DOM node, and an imperatively-added
  // class is lost on the new node while the effect (keyed only on
  // `flashedMessageId`) never re-runs. That was the "already-visible target
  // doesn't highlight" bug (#task:1076). Here we only hold the flag for the
  // 3s animation, then clear it and signal `onJumped` (the single jumpTarget
  // cleanup point). `flashedMessageId` is only ever set once the target is
  // present in `items` (completion gate above), so there is no "never
  // materialised" branch to guard.
  useEffect(() => {
    if (!flashedMessageId) return;
    const timer = setTimeout(() => {
      setFlashedMessageId(null);
      onJumped?.();
    }, 3000);
    return () => clearTimeout(timer);
  }, [flashedMessageId, onJumped]);

  // In-window find navigation (#task:1335) — the LOCAL half of "ensure the
  // match is visible" (Cindy). When the active find match is a row already in
  // the loaded window, scroll to it via the virtualizer INDEX (not a DOM
  // lookup: a loaded row may be unmounted by virtualization) — no jump, no
  // prime, so the list never remounts. That remount is the prev/next "white
  // flash"; off-window matches are the only ones that legitimately re-window,
  // and those come through `jumpToMessageId` (the branch below), which owns
  // positioning — so we bail while a jump is in flight. We only re-scroll when
  // the active id actually CHANGES (ref guard), never on unrelated `items`
  // churn (a new live message must not yank the reader back to the match).
  useEffect(() => {
    if (jumpToMessageId) return; // the jump/prime path owns scroll position
    const id = searchActiveMessageId;
    if (!id) {
      lastScrolledSearchIdRef.current = null;
      return;
    }
    if (id === lastScrolledSearchIdRef.current) return;
    const idx = items.findIndex((it) => it.kind === "message" && it.message.id === id);
    if (idx === -1) return; // not in the loaded window — ChannelView escalates it
    lastScrolledSearchIdRef.current = id;
    virtuosoRef.current?.scrollToIndex({
      index: idx,
      align: "center",
      behavior: "auto",
    });
  }, [searchActiveMessageId, items, jumpToMessageId]);

  const renderItem = useCallback(
    (item: ReturnType<typeof groupMessages>[number]) => {
      if (item.kind === "cluster") {
        return (
          <SystemActivityCluster
            messages={item.messages}
            conversationRefsById={conversationRefsById}
            onOpenConversationRef={onOpenConversationRef}
            channelsByName={channelsByName}
            onOpenChannelRef={onOpenChannelRef}
            humansByName={humansByName}
          />
        );
      }
      const m = item.message;
      // System rows are always committed; narrowing here keeps SystemMessage on
      // the plain MessageDto contract while pending rows fall through to the
      // message item below.
      if (isCommittedMessage(m) && m.from_kind === "system") {
        return (
          <SystemMessage
            message={m}
            conversationRefsById={conversationRefsById}
            onOpenConversationRef={onOpenConversationRef}
            channelsByName={channelsByName}
            onOpenChannelRef={onOpenChannelRef}
            humansByName={humansByName}
          />
        );
      }
      return (
        <ChannelMessageItem
          message={m}
          conversationId={channelId}
          canReact={canReact}
          canParticipate={canParticipate}
          canMutateLinkedTask={canMutateLinkedTask}
          agentsById={agentsById}
          agentsByName={agentsByName}
          humansByName={humansByName}
          humansById={humansById}
          channelsByName={channelsByName}
          onOpenChannelRef={onOpenChannelRef}
          conversationRefsById={conversationRefsById}
          onOpenConversationRef={onOpenConversationRef}
          onOpenAttachment={onOpenAttachment}
          {...actions.rowActionProps(m)}
          onStartThread={onOpenThread ? handleOpenThread : undefined}
          threadReplyCount={
            (replyCountsByRoot?.[m.id] ?? 0) > 0 ? replyCountsByRoot?.[m.id] : undefined
          }
          threadReplySummary={replySummariesByRoot?.[m.id]}
          threadUnread={threadUnreadByRoot?.[m.id]}
          threadReplyPreviews={replyPreviewsByRoot?.[m.id]}
          viewerAvatarObjectUrl={viewerAvatarObjectUrl}
          viewer={viewer}
          authorAgentAvatarUrls={authorAgentAvatarUrls}
          authorUserAvatarUrls={authorUserAvatarUrls}
          agentAvatarUrls={agentAvatarUrls}
          highlighted={highlightedMessageId != null && m.id === highlightedMessageId}
          jumpFlash={flashedMessageId != null && m.id === flashedMessageId}
          continuesPrevious={authorContinuations.has(m.id)}
          userResolver={userResolver}
          searchQuery={searchQuery}
          searchActive={searchActiveMessageId != null && m.id === searchActiveMessageId}
        />
      );
    },
    [
      channelId,
      canReact,
      agentsById,
      agentsByName,
      humansByName,
      channelsByName,
      onOpenChannelRef,
      conversationRefsById,
      onOpenConversationRef,
      onOpenAttachment,
      actions.rowActionProps,
      onOpenThread,
      handleOpenThread,
      replyCountsByRoot,
      replySummariesByRoot,
      threadUnreadByRoot,
      replyPreviewsByRoot,
      viewerAvatarObjectUrl,
      viewer,
      authorAgentAvatarUrls,
      authorUserAvatarUrls,
      agentAvatarUrls,
      highlightedMessageId,
      flashedMessageId,
      authorContinuations,
      userResolver,
      searchQuery,
      searchActiveMessageId,
    ],
  );

  return (
    <div className="relative min-h-0 min-w-0 flex-1 flex flex-col">
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
        selectionKey={channelId}
        channelTitle={channelTitle}
      />
      <div ref={viewportRef} className="relative min-h-0 min-w-0 flex-1">
        {isLoading ? (
          // Initial window fetch: the message skeleton (shared with the full-view
          // skeleton) rather than a "加载消息…" line, so entering a channel reads
          // the same as every other conversation-loading surface. `absolute
          // inset-0` fills this `relative` box.
          <MessageRowsSkeleton className="absolute inset-0" />
        ) : items.length === 0 && !leadingSlot ? (
          <EmptyConversation />
        ) : (
          <Virtuoso<(typeof items)[number], ListContext>
            // Remount on channel switches and full window replacements
            // (latest ↔ around-message). Prepends keep the same key so
            // Virtuoso can preserve scroll position; disjoint windows get
            // a fresh size/scroll queue and can use `initialTopMostItemIndex`.
            // `bottomAnchorVersion` also forces a remount for back-to-bottom
            // (declarative tail re-anchor) without clearing the selection.
            key={`${channelId}:${windowResetVersion}:${bottomAnchorVersion}`}
            ref={virtuosoRef}
            data={items}
            firstItemIndex={firstItemIndex}
            // On fresh mount, anchor the list at the jump target when
            // there is one (and the around-window has already landed in
            // `items` so we have a real index). Bottom fallback belongs
            // only to ordinary channel opens. During a jump, `idx === -1`
            // means "target not in this window yet"; falling back to the
            // local window bottom is how we ended up stuck on stale
            // around-window tails.
            {...(initialTopMostItemIndex !== undefined ? { initialTopMostItemIndex } : {})}
            // Stick to bottom only for the real tail. During a saved-message
            // jump, or while browsing an around-message window, the window's
            // bottom is just "the last row we currently hold" — following it
            // would override the target-centered jump and land on stale history.
            followOutput={(atBottom) =>
              getFollowOutputBehavior({
                hasJumpTarget: jumpToMessageId != null,
                isAtTail,
                atBottom,
              })
            }
            rangeChanged={({ startIndex }) => {
              const item = items[startIndex - firstItemIndex];
              if (item)
                viewMemory?.set(readingKey, { anchor: itemKey(item), atBottom: isAtBottom });
            }}
            atBottomStateChange={(atBottom) => {
              setIsAtBottom(atBottom);
              const previous = viewMemory?.get(readingKey);
              viewMemory?.set(readingKey, { ...previous, atBottom });
            }}
            atBottomThreshold={20}
            // On compact screens, correct variable row heights before the next
            // paint instead of showing the estimate for one more animation frame.
            // Virtuoso documents benign ResizeObserver loop notifications for
            // this mode; don't suppress global errors to hide them.
            skipAnimationFrameInResizeObserver={compact}
            startReached={handleStartReached}
            onScroll={(event) => {
              if (event.currentTarget.scrollTop <= viewportBudget.preload) handleStartReached();
            }}
            increaseViewportBy={viewportBudget.render}
            computeItemKey={(_index, item) => itemKey(item)}
            context={listContext}
            components={listComponents}
            itemContent={(_index, item) => (
              <div className="mx-auto max-w-300 px-(--message-list-gutter) py-0.5">
                {renderItem(item)}
              </div>
            )}
            className="h-full min-w-0"
          />
        )}

        {isFetchingEarlier ? (
          // Loading must not change the measured list header height: doing so
          // moves every message while the reader is waiting at the top.
          <div
            role="status"
            className="pointer-events-none absolute inset-x-0 top-(--pane-chrome-height,0px) text-center text-xs text-text-muted"
          >
            {t("conversationShell.loadingEarlierMessages")}
          </div>
        ) : null}

        {surfaceActive && (showScrollBottom || shouldResetToLatest) ? (
          <button
            type="button"
            data-scroll-bottom
            data-sticker
            data-sticker-size="lg"
            data-sticker-tone="neutral"
            aria-label={
              shouldResetToLatest
                ? t("conversationShell.jumpToLatest")
                : t("conversationShell.scrollToBottom")
            }
            title={
              shouldResetToLatest
                ? t("conversationShell.jumpToLatest")
                : t("conversationShell.scrollToBottom")
            }
            onClick={() => {
              // Back-to-bottom is ONE command: cancel the jump + arm the bottom
              // intent first (both branches), then obtain the tail window. The
              // two branches differ ONLY in how the tail is produced and which
              // lifecycle signal spends the intent — never in state ownership.
              // Not doing this split was the bug: the reset branch used to return
              // early, so the fresh latest window mounted with no armed intent
              // and stayed parked at the stale around-window position, far above
              // the tail.
              cancelJumpForBottom();
              if (shouldResetToLatest && onResetToLatest) {
                // Window is behind the tail. Reset the query to the authoritative
                // latest window; `resetQueries` empties the cache first, so
                // Virtuoso re-mounts naturally when rows return and reads the
                // armed intent. The reset request is spent by that rows-shown
                // mount (after the transient rows-hidden). Selection clears with
                // the discarded around-window, as any latest reset does.
                dispatchBottomAnchor({ type: "arm-reset" });
                onResetToLatest();
              } else {
                // Tail already loaded: remount in place at the last item. This
                // version is NOT a selection signal, so the message selection
                // survives. The tail request is spent once this remount commits.
                dispatchBottomAnchor({ type: "arm-tail" });
                setBottomAnchorVersion((v) => v + 1);
              }
            }}
            className="absolute bottom-4 left-1/2 z-(--z-sticky) flex size-9 -translate-x-1/2 items-center justify-center rounded-(--pill-radius) bg-accent text-text-on-accent shadow-lg transition-all hover:opacity-90 active:opacity-75"
          >
            <ArrowDownIcon className="size-4" />
          </button>
        ) : null}
      </div>
    </div>
  );
}
