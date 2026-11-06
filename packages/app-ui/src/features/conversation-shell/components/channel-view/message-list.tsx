import { useViewMemory } from "@/navigation/view-memory";
import { useCompactLayout } from "@/navigation/layout";
import { useEventCallback } from "@/hooks/use-event-callback";
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
import { ArrowDownIcon, GridLoaderIcon } from "@/icons";
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
import { groupMessages, type StreamItem } from "@/features/messages/model/group-messages";
import { useMessageActionSurface } from "@/features/messages/hooks/use-message-action-surface";
import { MessageActionChrome } from "@/features/messages/components/message-action-chrome";
import { NativeComposerRoom } from "@/features/messages/components/native-composer-room";
import { MessageRowsSkeleton } from "@/features/conversation-shell/components/conversation-view-skeleton";
import { EmptyConversation } from "./empty-conversation";
import {
  type CachedMessage,
  isCommittedMessage,
  isPendingMessage,
} from "@/features/channels/pending-message";
import type { WorkspaceChannelReference } from "@/features/messages/components/conversation-ref";
import {
  classifyTopShift,
  getInitialListAnchor,
  getMessageListPreloadDistance,
  INITIAL_BOTTOM_ANCHOR_STATE,
  reduceBottomAnchor,
  shouldFollowBottom,
  shouldRemountForWindowReplacement,
} from "@/features/conversation-shell/model/message-list-window";
import { MessageScroller, type MessageScrollerHandle } from "./message-scroller";
import { t } from "@/lib/i18n";

// ── Message list ────────────────────────────────────────────────────────

export function MessageList({
  channelId,
  workspaceId,
  messages,
  isLoading,
  hasMoreBefore,
  isFetchingEarlier,
  onLoadEarlier,
  hasMoreAfter = false,
  isFetchingLater = false,
  onLoadLater,
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
  /** Newer rows exist past the loaded window: an around-message jump, or a
   *  tail page dropped by the bounded window. */
  hasMoreAfter?: boolean;
  isFetchingLater?: boolean;
  onLoadLater?: () => void;
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
  const preloadDistance = getMessageListPreloadDistance(compact, viewportHeight);
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
  // The shell rebuilds these handlers on every navigation, and each new
  // identity would render every memoized row again. Rows get forwarders that
  // call the latest handler; only a handler appearing or disappearing reaches
  // them.
  const handleOpenThread = useEventCallback(
    (msg: CachedMessage, jumpToMessageId?: string): void => {
      // A pending optimistic row has no server id to anchor a thread on.
      if (!isCommittedMessage(msg)) return;
      onOpenThread?.(msg.id, jumpToMessageId);
    },
  );
  const openConversationRef = useEventCallback((id: string) => onOpenConversationRef?.(id));
  const openChannelRef = useEventCallback((id: string) => onOpenChannelRef?.(id));
  const openAttachment = useEventCallback((attachment: AttachmentSnapshot) =>
    onOpenAttachment?.(attachment),
  );
  const rowOpenConversationRef = onOpenConversationRef ? openConversationRef : undefined;
  const rowOpenChannelRef = onOpenChannelRef ? openChannelRef : undefined;
  const rowOpenAttachment = onOpenAttachment ? openAttachment : undefined;
  const rowStartThread = onOpenThread ? handleOpenThread : undefined;

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

  const scrollerRef = useRef<MessageScrollerHandle>(null);
  const [isAtBottom, setIsAtBottom] = useState(true);
  const [showScrollBottom, setShowScrollBottom] = useState(false);
  const [flashedMessageId, setFlashedMessageId] = useState<string | null>(null);
  // Last find match we locally scrolled to (see the in-window find effect
  // below). Guards against re-scrolling on every unrelated `items` change.
  const lastScrolledSearchIdRef = useRef<string | null>(null);
  // Bumped on a disjoint window replacement (latest ↔ around-message). It
  // remounts the scroller so the new window opens at its own anchor, and it
  // clears the message selection made in the window that went away.
  const [windowResetVersion, setWindowResetVersion] = useState(0);
  // Back-to-bottom when the window is behind the tail (see `reduceBottomAnchor`).
  const [bottomAnchor, dispatchBottomAnchor] = useReducer(
    reduceBottomAnchor,
    INITIAL_BOTTOM_ANCHOR_STATE,
  );
  // Shared message action surface — selection state + copy / share /
  // export handlers, identical to the thread reply-cluster pane
  // (#task:1067), so the per-row action props can't drift apart.
  const actions = useMessageActionSurface({
    channelId,
    windowResetVersion,
    jumpToMessageId,
    canSave,
  });
  // Channel TITLE — surfaced in the screenshot header and used as the suggested
  // filename base, and the only field the consumer reads.
  const channelTitle = useMemo(() => {
    const c =
      liveChannels.find((x) => x.id === channelId) ??
      archivedChannels.find((x) => x.id === channelId);
    return c ? c.title : null;
  }, [liveChannels, archivedChannels, channelId]);
  const [previousWindow, setPreviousWindow] = useState({ channelId, items });
  const lastJumpScrollKeyRef = useRef<string | null>(null);

  const itemKey = useCallback((item: StreamItem) => {
    return item.kind === "cluster" ? item.id : item.message.id;
  }, []);

  // Index of the current jump target inside `items`. -1 while App is still
  // priming the around-window.
  const jumpTargetIdx = useMemo(() => {
    if (!jumpToMessageId) return -1;
    return items.findIndex((it) => it.kind === "message" && it.message.id === jumpToMessageId);
  }, [items, jumpToMessageId]);

  // The reader is entering a fresh stream; the previous channel's state must
  // not leak in. `channelId` is already in the scroller key, so this does not
  // need to remount anything itself.
  useEffect(() => {
    dispatchBottomAnchor({ type: "cancel" });
    setIsAtBottom(true);
    setShowScrollBottom(false);
    setFlashedMessageId(null);
  }, [channelId]);

  // A fresh jump overrides any prior back-to-bottom request and the previous
  // attempt's flash. A layout effect, so the replacement remount below
  // already sees the cancelled intent and centers on the target.
  useLayoutEffect(() => {
    if (!jumpToMessageId) {
      lastJumpScrollKeyRef.current = null;
      return;
    }
    dispatchBottomAnchor({ type: "cancel" });
    setFlashedMessageId(null);
  }, [jumpToMessageId, channelId]);

  // Prepends and head-trims keep the scroller mounted: it holds the reader's
  // first visible row in place itself. Only a disjoint window remounts, and
  // during a jump only once it contains the target (`classifyTopShift`).
  //
  // Reconciled during render, so the scroller never commits a replacement
  // window under the old mount.
  if (previousWindow.channelId !== channelId) {
    setPreviousWindow({ channelId, items });
    setWindowResetVersion(0);
  } else if (previousWindow.items !== items) {
    setPreviousWindow({ channelId, items });
    const shift = classifyTopShift(previousWindow.items.map(itemKey), items.map(itemKey));
    if (
      shift.kind === "replace" &&
      shouldRemountForWindowReplacement({
        hasJumpTarget: jumpToMessageId != null,
        jumpTargetIndex: jumpTargetIdx,
        preferBottom: bottomAnchor.intent === "bottom",
      })
    ) {
      setWindowResetVersion((v) => v + 1);
    }
  }

  // Bring each resolved jump target into view once. A replacement window
  // already opened centered on it; a target inside the loaded window (a saved
  // message in this channel, say) is scrolled to in place.
  useLayoutEffect(() => {
    if (!jumpToMessageId || jumpTargetIdx < 0) {
      return;
    }
    const target = items[jumpTargetIdx];
    if (!target) return;
    const key = `${channelId}:${jumpToMessageId}`;
    if (lastJumpScrollKeyRef.current === key) return;
    lastJumpScrollKeyRef.current = key;
    scrollerRef.current?.scrollToKey(itemKey(target), "center");
  }, [channelId, items, itemKey, jumpTargetIdx, jumpToMessageId]);

  // The reset's latest window is spent by its first real mount, after the
  // transient empty `resetQueries` produces.
  const hasRenderedRows = !isLoading && items.length > 0;
  useLayoutEffect(() => {
    dispatchBottomAnchor({ type: hasRenderedRows ? "rows-shown" : "rows-hidden" });
  }, [hasRenderedRows]);

  useEffect(() => {
    setShowScrollBottom(!isAtBottom);
  }, [isAtBottom]);

  // The reader's own send is brought into view wherever they were reading.
  // Others' messages only follow a reader already at the bottom (`follow`).
  // Optimistic rows exist only for the viewer's own sends.
  const lastItem = items[items.length - 1];
  const ownSendKey =
    lastItem?.kind === "message" && isPendingMessage(lastItem.message) ? lastItem.message.id : null;
  const followedSendRef = useRef(ownSendKey);
  useLayoutEffect(() => {
    if (ownSendKey && ownSendKey !== followedSendRef.current) {
      scrollerRef.current?.scrollToBottom();
    }
    followedSendRef.current = ownSendKey;
  }, [ownSendKey]);

  const handleStartReached = useCallback(() => {
    if (!surfaceActive || jumpToMessageId || !hasMoreBefore || isFetchingEarlier) return;
    onLoadEarlier();
  }, [surfaceActive, jumpToMessageId, hasMoreBefore, isFetchingEarlier, onLoadEarlier]);

  const handleEndReached = useCallback(() => {
    if (!surfaceActive || jumpToMessageId || !hasMoreAfter || isFetchingLater) return;
    onLoadLater?.();
  }, [surfaceActive, jumpToMessageId, hasMoreAfter, isFetchingLater, onLoadLater]);

  const isAtBottomRef = useRef(isAtBottom);
  isAtBottomRef.current = isAtBottom;
  const handleTopItemChange = useCallback(
    (key: string) => {
      viewMemory?.set(readingKey, { anchor: key, atBottom: isAtBottomRef.current });
    },
    [viewMemory, readingKey],
  );
  const handleAtBottomChange = useCallback(
    (atBottom: boolean) => {
      setIsAtBottom(atBottom);
      const previous = viewMemory?.get(readingKey);
      viewMemory?.set(readingKey, { ...previous, atBottom });
    },
    [viewMemory, readingKey],
  );

  // Back-to-bottom cancels the jump on both sides: `onJumped()` clears the
  // parent's target, and clearing `flashedMessageId` cancels the local flash
  // so scrolling back up does not show the highlight of a cancelled jump.
  const cancelJumpForBottom = useCallback(() => {
    if (jumpToMessageId) onJumped?.();
    setFlashedMessageId(null);
    setShowScrollBottom(false);
  }, [jumpToMessageId, onJumped]);
  const shouldResetToLatest = !isAtTail || hasNewerLiveGap;

  // Read by the scroller only when it mounts.
  const restoreKey =
    windowResetVersion === 0 && savedPosition?.anchor && !savedPosition.atBottom
      ? savedPosition.anchor
      : null;
  const initialAnchor = getInitialListAnchor({
    jumpTargetKey: jumpTargetIdx >= 0 ? itemKey(items[jumpTargetIdx]!) : null,
    restoreKey,
    preferBottom: bottomAnchor.intent === "bottom",
  });

  // Jump-to-message completion gate. Positioning happens above; this effect
  // only decides whether to wait for prime, give up on a final miss, or start
  // the one-shot flash/onJumped sequence.
  //
  // `jumpPrimePending` tells "target not in cache yet" (App is priming an
  // around-window — wait for the next render) from "primed and not present"
  // (App clears `jumpTarget` on 404; for any other miss, signal `onJumped` so
  // the parent doesn't sit on a stuck target).
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

  // One-shot flash lifecycle. The flash visual is declarative — the row
  // renders `.msg-jump-flash` from its `jumpFlash` prop, so a remount that
  // replaces the target's DOM node keeps the animation (#task:1076). Here we
  // only hold the flag for the 3s animation, then clear it and signal
  // `onJumped` (the single jumpTarget cleanup point).
  useEffect(() => {
    if (!flashedMessageId) return;
    const timer = setTimeout(() => {
      setFlashedMessageId(null);
      onJumped?.();
    }, 3000);
    return () => clearTimeout(timer);
  }, [flashedMessageId, onJumped]);

  // In-window find navigation (#task:1335). An active match already in the
  // loaded window is scrolled to in place — no jump, no prime, no remount.
  // Off-window matches come through `jumpToMessageId`, which owns
  // positioning, so we bail while a jump is in flight. We only re-scroll when
  // the active id actually CHANGES, never on unrelated `items` churn (a new
  // live message must not yank the reader back to the match).
  useEffect(() => {
    if (jumpToMessageId) return;
    const id = searchActiveMessageId;
    if (!id) {
      lastScrolledSearchIdRef.current = null;
      return;
    }
    if (id === lastScrolledSearchIdRef.current) return;
    const item = items.find((it) => it.kind === "message" && it.message.id === id);
    if (!item) return; // not in the loaded window — ChannelView escalates it
    lastScrolledSearchIdRef.current = id;
    scrollerRef.current?.scrollToKey(itemKey(item), "center");
  }, [searchActiveMessageId, items, itemKey, jumpToMessageId]);

  const renderItem = useCallback(
    (item: StreamItem) => {
      if (item.kind === "cluster") {
        return (
          <SystemActivityCluster
            messages={item.messages}
            conversationRefsById={conversationRefsById}
            onOpenConversationRef={rowOpenConversationRef}
            channelsByName={channelsByName}
            onOpenChannelRef={rowOpenChannelRef}
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
            onOpenConversationRef={rowOpenConversationRef}
            channelsByName={channelsByName}
            onOpenChannelRef={rowOpenChannelRef}
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
          onOpenChannelRef={rowOpenChannelRef}
          conversationRefsById={conversationRefsById}
          onOpenConversationRef={rowOpenConversationRef}
          onOpenAttachment={rowOpenAttachment}
          {...actions.rowActionProps(m)}
          onStartThread={rowStartThread}
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
      rowOpenChannelRef,
      conversationRefsById,
      rowOpenConversationRef,
      rowOpenAttachment,
      actions.rowActionProps,
      rowStartThread,
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

  const renderRow = useCallback(
    (item: StreamItem) => (
      <div className="mx-auto max-w-300 px-(--message-list-gutter) py-0.5">{renderItem(item)}</div>
    ),
    [renderItem],
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
          // skeleton), so entering a channel reads the same as every other
          // conversation-loading surface. `absolute inset-0` fills this box.
          <MessageRowsSkeleton className="absolute inset-0" />
        ) : items.length === 0 && !leadingSlot ? (
          <EmptyConversation />
        ) : (
          <MessageScroller<StreamItem>
            // Remount on channel switches and disjoint window replacements so
            // the new window opens at its own anchor.
            key={`${channelId}:${windowResetVersion}`}
            controlRef={scrollerRef}
            items={items}
            itemKey={itemKey}
            renderItem={renderRow}
            header={
              <>
                {/* Reserves the floating compact header's height inside the
                scroller, so rows scroll beneath it. */}
                <div aria-hidden data-pane-chrome-spacer />
                {leadingSlot ? (
                  <div className="mx-auto mb-1 max-w-300 px-(--message-list-gutter)">
                    {leadingSlot}
                  </div>
                ) : null}
              </>
            }
            footer={
              <>
                <div className="h-3" data-composer-activity-room />
                <NativeComposerRoom />
              </>
            }
            initialAnchor={initialAnchor}
            // Follow new rows only at the real conversation tail: during a jump,
            // or in an around-message window, the bottom is just the last row
            // currently held.
            follow={shouldFollowBottom({ hasJumpTarget: jumpToMessageId != null, isAtTail })}
            atBottomThreshold={20}
            reachEdgeDistance={preloadDistance}
            onReachTop={handleStartReached}
            onReachBottom={handleEndReached}
            onAtBottomChange={handleAtBottomChange}
            onTopItemChange={handleTopItemChange}
            className="h-full min-w-0"
          />
        )}

        {isFetchingEarlier ? (
          // Loading must not change the measured list header height: doing so
          // moves every message while the reader is waiting at the top.
          <div className="pointer-events-none absolute inset-x-0 top-[calc(var(--pane-chrome-height,0px)+12px)] flex justify-center motion-safe:animate-in motion-safe:fade-in-0">
            <div role="status" className="flex items-center gap-1.5 text-xs text-text-muted">
              <GridLoaderIcon className="size-3.5" />
              {t("conversationShell.loadingEarlierMessages")}
            </div>
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
              cancelJumpForBottom();
              if (shouldResetToLatest && onResetToLatest) {
                // The window is behind the tail: reset the query to the latest
                // window, which mounts at its bottom (the armed intent).
                dispatchBottomAnchor({ type: "arm-reset" });
                onResetToLatest();
              } else {
                scrollerRef.current?.scrollToBottom();
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
