import { useCallback, useMemo, useRef, useState } from "react";
import { paneChrome } from "@/components/layout/pane-chrome";
import { ArrowUpRightIcon, BellIcon, BellOffIcon, SearchIcon, ThreadIcon } from "@/icons";
import { InlineFindBar } from "./inline-find-bar";
import { useInlineFindController, useInlineFindHotkey } from "@/features/search/use-inline-find";
import type { AdvancedSearchSeed } from "@/features/search/advanced-search-seed";
import type { SenderFilterValue } from "@/features/search/sender-filter-select";
import { Composer } from "@/features/messages/components/composer";
import { SelectionQuoteToolbar } from "@/features/messages/components/selection-quote-toolbar";
import { ChannelJoinBar } from "@/features/messages/components/channel-join-bar";
import { MessageRowsSkeleton } from "@/features/conversation-shell/components/conversation-view-skeleton";
import { SidePanelHeader, SidePanelHeaderTitle } from "@/components/layout/side-panel-header";
import { iconMarkup, type PageBarDeclaration } from "@/components/layout/native-page-bar";
import { useMessageRenderContext } from "@/features/messages/hooks/use-message-render-context";
import { threadClusterShowsLoader, threadReplyRows } from "../model/thread-mark-read";
import { useThreadMarkRead } from "../use-thread-mark-read";
import { MessageList } from "./channel-view/message-list";
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
  useThreadCluster,
  useThreadFollowers,
  useThreadRoot,
  useUnfollowThread,
} from "@/features/channels/hooks";
import type { AttachmentSnapshot } from "@silverhand/shared";
import type { ComposerDirectory } from "@/features/conversations/pane-mode";
import { useAgents } from "@/features/agents/hooks/hooks";
import { useAgentAvatarObjectUrls } from "@/features/agents/hooks/avatar-hooks";
import { useJoinChannel } from "@/features/workspaces/hooks";
import type { ChannelGate } from "@/features/channels/membership";
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
  // A cluster can be mid-history after an around-reply jump, so the list's
  // back-to-bottom resets to the true tail rather than the window's end.
  const { isAtTail, hasNewerLiveGap, resetToLatest, windowRows, trimWindow } = cluster;

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
  const followPending = followThread.isPending || unfollowThread.isPending;
  // Join the PARENT channel (the Composer-slot affordance for a non-member
  // browsing this public channel's thread).
  const joinChannel = useJoinChannel(workspaceId);
  const toggleFollow = (): void => {
    if (followPending) return;
    if (viewerFollowing) unfollowThread.mutate();
    else followThread.mutate();
  };

  const { liveChannels, archivedChannels } = useMessageRenderContext(
    workspaceId,
    channelId,
    canParticipate,
  );
  // The directory avatar batch for inline `@handle` pills; bylines resolve
  // from each row's own author inside the list.
  const { data: agents = [] } = useAgents();
  const agentAvatarUrls = useAgentAvatarObjectUrls(agents);

  // The join bar names the parent channel; fall back to the `parentTitle` prop
  // while the channel list resolves.
  const parentChannelTitle = useMemo(() => {
    const c =
      liveChannels.find((x) => x.id === channelId) ??
      archivedChannels.find((x) => x.id === channelId);
    return c ? c.title : (parentTitle ?? null);
  }, [liveChannels, archivedChannels, channelId, parentTitle]);

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

  // The root opens the list (`leadingMessage`), so it is dropped from the
  // replies: it is in the cluster window whenever the window reaches the
  // thread's start. `threadReplyRows` is the single authority for "which window
  // rows are replies"; the read cursor derives from the same predicate.
  const replyMessages = useMemo(
    () => threadReplyRows(messages, rootMessageId),
    [messages, rootMessageId],
  );

  // Authoritative full reply count from the cut3-6 sidecar (kept live by
  // useThreadRoot) — NOT a count of currently-loaded rows, which would be
  // wrong for a paginated long thread.
  const replyCount = rootCtx.replyCount;

  const { hasPreviousPage, isFetchingPreviousPage, fetchPreviousPage } = cluster;
  const { hasNextPage, isFetchingNextPage, fetchNextPage } = cluster;
  const loadEarlier = useCallback(() => {
    if (!hasPreviousPage || isFetchingPreviousPage) return;
    void fetchPreviousPage({ cancelRefetch: false });
  }, [fetchPreviousPage, hasPreviousPage, isFetchingPreviousPage]);
  const loadLater = useCallback(() => {
    if (!hasNextPage || isFetchingNextPage) return;
    void fetchNextPage({ cancelRefetch: false });
  }, [fetchNextPage, hasNextPage, isFetchingNextPage]);

  const listRef = useRef<HTMLDivElement>(null);
  const [atBottom, setAtBottom] = useState(true);
  const getViewport = useCallback(
    () => listRef.current?.querySelector<HTMLElement>("[data-message-scroller]") ?? null,
    [],
  );

  // In-conversation header find (#task:1335), scoped to THIS thread stream
  // (stream='thread' + thread_root_id). Navigation is "ensure the match is
  // visible", NOT "reopen a window" (Cindy):
  //   • a loaded reply → `MessageList` scrolls to the active match itself.
  //   • the root → scrolled to here; it opens the list, outside its rows.
  //   • off the loaded window → App's jump/prime path (carrying
  //     `threadRootId`) primes the cluster window, and the list locates and
  //     flashes the reply once it lands.
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
      if (messageId === rootMessageId) {
        // Scoped to THIS pane: the root is also rendered in the channel with
        // the same `id="msg-<root>"`, so a global lookup would scroll the
        // wrong pane (Cindy).
        listRef.current
          ?.querySelector(`[id="msg-${messageId}"]`)
          ?.scrollIntoView({ block: "center", behavior: "smooth" });
        return;
      }
      if (replyMessages.some((m) => m.id === messageId)) return;
      onJumpToMessage?.({
        workspaceId: workspaceId ?? undefined,
        channelId,
        threadRootId: rootMessageId,
        messageId,
      });
    },
    [replyMessages, onJumpToMessage, workspaceId, channelId, rootMessageId],
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

  useThreadMarkRead({
    channelId,
    rootMessageId,
    jumpMessageId: jumpMessageId ?? null,
    canParticipate,
    cluster,
    messages,
    isAtTail,
    getViewport,
    scrollTrigger: atBottom,
  });

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
          <div ref={listRef} className="relative flex min-h-0 flex-1 flex-col">
            <MessageList
              key={rootMessageId}
              channelId={channelId}
              workspaceId={workspaceId}
              streamKey={`${channelId}#cluster#${rootMessageId}`}
              messages={replyMessages}
              leadingMessage={rootCtx.root ?? null}
              emptyState={
                <div className="flex flex-1 items-center justify-center px-4 py-6 text-center text-xs text-text-muted">
                  {t("conversationShell.noRepliesYet")}
                </div>
              }
              isLoading={threadClusterShowsLoader({ isLoading: cluster.isLoading, jumpPhase })}
              hasMoreBefore={!!hasPreviousPage}
              isFetchingEarlier={isFetchingPreviousPage}
              onLoadEarlier={loadEarlier}
              hasMoreAfter={!!hasNextPage}
              isFetchingLater={isFetchingNextPage}
              onLoadLater={loadLater}
              windowRows={windowRows}
              onTrimWindow={trimWindow}
              agentAvatarUrls={agentAvatarUrls}
              // Located only once this open's around-window is authoritative:
              // a stale window still on screen while priming must not start the
              // flash, whose `onJumped` releases the window lock (#task:1317).
              jumpToMessageId={threadLocateAllowed(jumpPhase) ? (jumpMessageId ?? null) : null}
              onJumped={onJumped}
              isAtTail={isAtTail}
              hasNewerLiveGap={hasNewerLiveGap}
              onResetToLatest={resetToLatest}
              onAtBottomChange={setAtBottom}
              conversationRefsById={conversationRefsById}
              onOpenConversationRef={onOpenConversationRef}
              onOpenAttachment={onOpenAttachment}
              onOpenChannelRef={handleOpenChannelRef}
              canReact={!closed && !archived && canParticipate}
              canSave={canParticipate}
              canParticipate={canParticipate}
              canMutateLinkedTask={!closed && !archived && gate.allowMutations}
              searchQuery={find.highlightQuery}
              searchActiveMessageId={find.activeMessageId}
            />
          </div>
        )}

        <SelectionQuoteToolbar
          boundaryRef={listRef}
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
