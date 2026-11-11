import { useCompactLayout } from "@/navigation/layout";
import { MobileNavigationSlot } from "@/components/layout/mobile-header-slot";
import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  AttachmentIcon,
  CloseIcon,
  CollapseIcon,
  ExpandIcon,
  MoreIcon,
  SearchIcon,
  SettingsIcon,
  ThreadIcon,
} from "@/icons";
import { Composer } from "@/features/messages/components/composer";
import { SelectionQuoteToolbar } from "@/features/messages/components/selection-quote-toolbar";
import { ChannelJoinBar } from "@/features/messages/components/channel-join-bar";
import { QuotedMessagePreview } from "@/features/messages/components/quoted-message-preview";
import { openAttachmentExternally } from "@/features/messages/model/open-attachment";
import {
  attachmentPreviewKind,
  resolveAttachmentOpenRoute,
} from "@/features/attachments/attachment-preview";
import { ConversationViewSkeleton } from "@/features/conversation-shell/components/conversation-view-skeleton";
import { AddConversationMemberPopover } from "@/features/conversations/add-conversation-member-popover";
import { TaskRefContextProvider } from "@/features/tasks/components/task-ref-context";
import { useNav, useRouteState } from "@/router";
import { useAgents } from "@/features/agents/hooks/hooks";
import { useAgentAvatarObjectUrls } from "@/features/agents/hooks/avatar-hooks";
import {
  useChannelAgents,
  useChannelAgentRuntime,
  useConversationUnread,
} from "@/features/channels/hooks";
import { useChannelMessageWindow } from "../use-channel-message-window";
import { useChannelMembership } from "@/features/channels/use-channel-membership";
import { useJoinChannel } from "@/features/workspaces/hooks";
import type { WorkspaceChannelReference } from "@/features/messages/components/conversation-ref";
import { MemberStrip } from "./channel-view/member-strip";
import { MessageList } from "./channel-view/message-list";
import { InlineFindBar } from "./inline-find-bar";
import { useInlineFindController, useInlineFindHotkey } from "@/features/search/use-inline-find";
import {
  directPairSenderSeed,
  type AdvancedSearchSeed,
} from "@/features/search/advanced-search-seed";
import type { MessageJumpTarget } from "@/features/channels/use-message-jump-controller";
import type { AttachmentSnapshot } from "@silverhand/shared";
import type { ConversationPaneDescriptor } from "@/features/conversations/pane-descriptor";
import {
  composerDirectory,
  paneCapabilities,
  projectConversationPaneMode,
  rosterQueryTarget,
} from "@/features/conversations/pane-mode";
import { ConversationPaneCapabilitiesProvider } from "@/features/conversations/pane-capabilities-context";
import { focusAgentSideSection } from "@/features/agents/components/agent-view";
import {
  ConversationHeaderIdentity,
  useHeaderIdentityIcon,
} from "./channel-view/conversation-header-identity";
import { paneChrome } from "../../../components/layout/pane-chrome";
import { FloatingPaneChrome } from "@/components/layout/pane-shell";
import { cn } from "@/lib/cn";
import {
  ConversationTabBar,
  panelPresentation,
  tabPresentation,
} from "./channel-view/conversation-tab-bar";
import {
  iconMarkup,
  useNativeBar,
  useNativeBarPlatform,
  type PageBarDeclaration,
} from "@/components/layout/native-page-bar";
import { ChannelTasksTab } from "./channel-view/channel-tasks-tab";
import { ChannelAttachmentTab } from "./channel-view/channel-attachment-tab";
import { ConversationPanelTab } from "./channel-view/conversation-panel-tab";
import { useConversationTabs } from "@/features/conversations/use-conversation-tabs";
import {
  isRemovable,
  tabKey,
  type PanelTabKind,
} from "@/features/conversation-shell/model/conversation-tabs";
import {
  taskBoardContainerForPane,
  type TaskBoardContainer,
} from "@/features/tasks/model/task-container";
import { t, useLocale } from "@/lib/i18n";

interface ChannelViewProps {
  /** Whether this conversation's container has a `#` channel namespace: true for
   *  any resolved workspace, false BOTH for a workspace-less conversation (a
   *  `direct_pair`, #task:2035) and for a workspace whose row hasn't resolved.
   *  Both must withhold every channel-like affordance, so one `false` serves.
   *  This used to be the parent workspace's KIND, but no consumer under here ever
   *  told the two kinds apart — the divergences are all "is there a channel
   *  namespace at all" (#task:2078). */
  hasChannelNamespace: boolean;
  /** Display name of this conversation's workspace, resolved ONCE by the pane
   *  owner from its id→name authority and threaded down as a sibling to
   *  `hasChannelNamespace` — never looked up in this leaf (#task:2126). The
   *  Composer's `#` picker spells a semantic channel locator with it. Null in a
   *  WIDER set than `workspace_id` is null: a workspace-less conversation, an
   *  unresolved row, AND a resolved Agent Space (whose name the locator grammar
   *  cannot spell — see `locatorWorkspaceName`). Every null falls the picker back
   *  to `#conv:<id>`. */
  workspaceName: string | null;
  conversation: ConversationPaneDescriptor | null;
  /** True when a channel IS selected in the route but its workspace's channel
   *  list hasn't resolved yet (e.g. a cross-workspace jump from Activity's
   *  "back to source"): `conversation` is transiently null. Distinguishes this
   *  loading window from "no channel selected" so the pane shows a loader
   *  instead of the misleading "select a channel" empty state (#task:1269). */
  resolving?: boolean;
  /** True when the read that would resolve `conversation` FAILED and left
   *  nothing — a third state beside "still resolving" and "nothing selected"
   *  (#task:2035). Both of the others lie about a failure: the skeleton promises
   *  an answer that is not coming, and "Select a channel" claims the viewer
   *  chose nothing. Only consulted when `conversation` is null and `resolving`
   *  is false, so an in-flight retry keeps showing the loader. */
  loadFailed?: boolean;
  /** True when the authority ANSWERED and the route's conversation is not one
   *  the viewer can open — today the `direct_pair` arm's settled absence: a bad
   *  or stale `/chat/d/:id`, or a pair the viewer is no longer an endpoint of
   *  (#task:2035). A FOURTH state, because "Select a channel" would claim the
   *  viewer chose nothing while the route names a conversation, and the loader
   *  would promise an answer that has already arrived. Only consulted when
   *  `conversation` is null and neither `resolving` nor `loadFailed` holds. */
  notFound?: boolean;
  parentTitle?: string | null;
  /** One-shot scroll-to target. When set and the target message is in
   *  the current channel, MessageList scrolls + flashes it, then calls
   *  `onJumped` so the parent can clear the target (otherwise switching
   *  workspaces and back would re-trigger the scroll). */
  jumpTarget?: { channelId: string; messageId: string } | null;
  /** When true, App is still fetching an `around_message_id` window
   *  for the current `jumpTarget` and the message may not yet be in
   *  cache. MessageList's jump effect uses this to distinguish
   *  "still priming, wait" from "primed and not present, give up"
   *  on `idx === -1`. Bound by App to the latest jump attempt's
   *  `(channelId, messageId)`; superseded by any newer jump. */
  jumpPrimePending?: boolean;
  onJumped?: () => void;
  /** Called when the user clicks Thread / the reply badge on a main-line
   *  message — opens that message's thread (reply cluster) in the right
   *  pane. Receives the root message id. When undefined the Thread action
   *  is hidden (secondary panes can't open nested threads). */
  onOpenThread?: (rootMessageId: string, jumpToMessageId?: string) => void;
  /** Toggle the channel Threads-list pane (#task:1073) — opens when
   *  closed, closes when open (#task:1126 follow-up). When set, the header
   *  renders a Threads toggle. Only the primary channel pane passes this —
   *  thread/dm/side panes have no channel-scoped thread index. */
  onToggleThreadsList?: () => void;
  /** Whether the Threads-list pane is currently open (drives the toggle's
   *  active styling + aria-pressed). */
  threadsListActive?: boolean;
  /** Toggle the channel Attachments-list pane — opens when
   *  closed, closes when open. When set, the header renders an Attachments
   *  toggle. Only the primary channel pane passes this — thread/dm/side
   *  panes have no channel-scoped attachments index. */
  onToggleFilesList?: () => void;
  /** Whether the Attachments-list pane is currently open (drives the
   *  toggle's active styling + aria-pressed). */
  filesListActive?: boolean;
  /** When set, renders an X button in the header that calls this back.
   *  Used by the right-side secondary pane to close itself. */
  onCloseChild?: () => void;
  /** Localized aria/title text for the close button. Defaults to
   *  `'关闭 Thread'` for back-compat. PR-1 secondary pane is generic
   *  (channel/thread/dm), so the caller decides — `'关闭 Thread'` for
   *  thread panes, `'关闭侧栏'` for `?side=<channel>` panes. Ignored
   *  when `onCloseChild` is undefined. */
  closeLabel?: string;
  /** "View in channel" handler — for thread panes, jumps the parent
   *  pane to the original anchor message. Receives the original
   *  message id (which lives in the thread_anchor system message). */
  onJumpToOriginal?: (messageId: string) => void;
  /** Reports this channel's thread anchor target back up so the caller
   *  can persistently highlight that message in the parent pane while
   *  this view is open. Fires with the id when known, null otherwise. */
  onThreadAnchorResolved?: (originalMessageId: string | null) => void;
  /** Persistently mark a message in this view's list with the hover
   *  frame — used by the parent pane to surface "the message a thread
   *  is anchored to". */
  highlightedMessageId?: string | null;
  /** Toggle thread fullscreen (parent pane hidden). Only meaningful
   *  alongside `onCloseChild` (i.e. when this view is the thread pane). */
  onToggleFullscreen?: () => void;
  /** Whether the thread pane is currently fullscreen. Drives the icon
   *  toggle (Maximize2 ↔ Minimize2). */
  isFullscreen?: boolean;
  /** Current-workspace top-level conversation id map for inline `#conv:<id>` refs. */
  conversationRefsById?: Map<string, WorkspaceChannelReference>;
  onOpenConversationRef?: (id: string) => void;
  /** Click handler for inline `#<channel_name>` chips. The chip's
   *  resolution scope is this conversation's workspace, so the caller
   *  binds the workspace id and we hand it the resolved channel id. */
  onOpenChannelRef?: (workspaceId: string, channelId: string) => void;
  /** Opens a task's detail in the side pane. Receives the RESOLVED task id.
   *  Both an inline `#task:<n>` chip (which resolves `#<n>` against this
   *  conversation's workspace, so no workspace axis travels with the click) and
   *  a Tasks-tab card route through here, so a card opens the same side-pane
   *  host a chip does. */
  onOpenTask?: (taskId: string) => void;
  /** In-conversation header find (#task:1335). When provided, the header shows
   *  a search-toggle icon that opens the inline find bar below it. The callback
   *  navigates to a prev/next match in THIS channel via App's shared jump/prime
   *  path (`handleJumpToMessage`) — so off-window matches are materialised, not
   *  skipped. Absent → no find affordance (e.g. secondary panes, which don't
   *  support being jumped into). */
  onJumpToMessage?: (params: MessageJumpTarget) => void;
  /** Opens the advanced `SearchPanel` with this pane's own coordinate translated
   *  into the axes that panel HAS (shiki #task:1335, #task:2114): a channel
   *  seeds its workspace, a `direct_pair` seeds its counterpart as the sender.
   *  See `AdvancedSearchSeed` for why a pair cannot seed a container. */
  onOpenAdvancedSearch?: (seed: AdvancedSearchSeed) => void;
  /** Enables the per-conversation view tab bar (#task:2121) — Chat / Tasks (+
   *  the viewer's custom tabs) under the header, with the Tasks board hosted
   *  inline. Only the PRIMARY pane passes it: a thread / side pane is a child
   *  view of the same container and gets no tab strip of its own. */
  viewTabsEnabled?: boolean;
  /** One-shot request to open an attachment from a SIBLING pane (the Attachments
   *  list, #task:2122) into THIS primary pane. The list can't reach this pane's
   *  tab controller directly, so it hands the attachment up to App, which relays
   *  it here. The request carries the conversation it was ISSUED from; the
   *  consumer runs `resolveAttachmentOpenRoute` against the conversation actually
   *  mounted now, so a request from A that lands after a switch to B is DROPPED,
   *  never routed into B's tabs or OS-opened on B. Where the source matches it
   *  uses the SAME rule the message card does — `attachmentPreviewKind` + the
   *  tab-strip gate — so md/html land in the previewer tab where this pane hosts
   *  tabs, and everything else falls back to open-with-OS. A fresh object per
   *  click (App wraps it) so re-clicking the same file re-fires. */
  openAttachmentRequest?: {
    conversationId: string;
    attachment: AttachmentSnapshot;
  } | null;
  /** Clear the one-shot above once consumed. */
  onAttachmentRequestConsumed?: () => void;
}

export const ChannelView = memo(function ChannelView(props: ChannelViewProps): JSX.Element {
  useLocale();
  if (!props.conversation) {
    if (props.resolving) {
      // A channel IS selected but its descriptor is still resolving (a
      // cross-workspace jump waiting on the channels list). Show the full
      // conversation skeleton — same header/messages/composer shape the live
      // view will take — so the swap never jumps and we never flash the
      // "no channel" empty text during the 1-2s resolve.
      return <ConversationViewSkeleton showTabs={props.viewTabsEnabled} />;
    }
    if (props.loadFailed) {
      // Resolved to a FAILURE, not to an absence: say so plainly. Re-entering
      // the conversation remounts the query, which refetches an errored key, so
      // "reopen" is a real retry rather than advice we can't honour.
      return (
        <main
          data-panel="chat"
          className="flex h-full min-w-0 flex-1 items-center justify-center text-sm text-text-muted"
          style={paneChrome}
        >
          {t("conversationShell.couldntLoadThisConversation")}
        </main>
      );
    }
    if (props.notFound) {
      // Resolved to an ANSWERED absence: the route names a conversation and the
      // authority says this viewer cannot open it. Neither the loader nor
      // "Select a channel" is true here.
      return (
        <main
          data-panel="chat"
          className="flex h-full min-w-0 flex-1 items-center justify-center text-sm text-text-muted"
          style={paneChrome}
        >
          {t("conversationShell.thisConversationIsntAvailable")}
        </main>
      );
    }
    return (
      <main
        data-panel="chat"
        className="flex h-full min-w-0 flex-1 items-center justify-center text-sm text-text-muted"
        style={paneChrome}
      >
        {t("conversationShell.selectAChannel")}
      </main>
    );
  }
  return <ActiveChannelView {...props} conversation={props.conversation} />;
});

/**
 * Show the "+ add member" popover when the conversation can take
 * one. Regular channels support adding; DM channels are single-pair
 * by definition. Closed conversations are gated out — the route would
 * also reject with `conversation_closed`, but hiding the trigger keeps
 * the surface intentional.
 */
function canAddMember(conv: { roster_policy: "open" | "fixed" }, closed: boolean): boolean {
  if (closed) return false;
  return conv.roster_policy === "open";
}

// Split out so the React-Query hooks below only mount once a channel
// exists — keeps hook order stable across the null transition.
function ActiveChannelView({
  hasChannelNamespace,
  workspaceName,
  conversation,
  parentTitle,
  jumpTarget,
  jumpPrimePending = false,
  onJumped,
  onOpenThread,
  onToggleThreadsList,
  threadsListActive = false,
  onToggleFilesList,
  filesListActive = false,
  onCloseChild,
  closeLabel = t("conversationShell.closeThread"),
  onJumpToOriginal,
  onThreadAnchorResolved,
  highlightedMessageId,
  onToggleFullscreen,
  isFullscreen,
  conversationRefsById,
  onOpenConversationRef,
  onOpenChannelRef,
  onOpenTask,
  onJumpToMessage,
  onOpenAdvancedSearch,
  viewTabsEnabled = false,
  openAttachmentRequest,
  onAttachmentRequestConsumed,
}: ChannelViewProps & { conversation: ConversationPaneDescriptor }): JSX.Element {
  // Per-viewer membership gate. A PUBLIC channel is readable by a
  // workspace user who has not joined it: they browse (message history + thread
  // content + inline attachment preview) with a Join affordance in place of the
  // composer, and every member effect / mutation is withheld. The three-state
  // (member | nonmember | unknown) is resolved by the shared hook from the
  // carrier's own authoritative `is_member` (or a channel-detail fetch when a
  // deep-link carrier lacks it). The gate is FAIL-CLOSED: until it resolves to
  // `member`, no member effect fires and no write affordance renders. DMs and
  // private channels always resolve to `member` (the backend stamps
  // `is_member: true`), so this is a no-op regression-wise there.
  //
  // A `direct_pair` takes the third argument instead (#task:2035): it has no
  // membership axis, no `is_member` carrier and no channel detail to fetch — see
  // `directPairGate`.
  // The pane's MODE, projected ONCE from the conversation's placement
  // (#task:2039). Every placement-dependent affordance below reads
  // `paneCaps` instead of re-deriving the mode from `workspace_id === null`, the
  // route, or a local boolean — that scatter is what produced four separate
  // dogfood findings. It decides UI capability only; authorization stays the
  // backend's answer (@Cindy).
  const paneMode = projectConversationPaneMode(conversation);
  const paneCaps = paneCapabilities(paneMode);
  // The composer's candidate directory, projected from the SAME typed mode (never
  // re-derived in the leaf). The workspace coordinate the workspace arm carries is
  // this pane's already-resolved name + namespace bit (#task:2147).
  const composerDir = composerDirectory(paneMode, {
    workspaceName,
    hasChannelNamespace,
  });
  const isDirectPair = paneMode.kind === "direct_message";
  const { gate } = useChannelMembership(
    conversation,
    conversation.id,
    isDirectPair ? { readOnly: conversation.read_only === true } : null,
  );
  const joinChannel = useJoinChannel(conversation.workspace_id);
  // WHETHER this pane reads the roster at all, resolved once from the capability
  // and the membership gate (`rosterQueryTarget`). Non-null is also the single
  // gate for every roster SURFACE below, so the reads and the affordances cannot
  // drift apart: a pane that shows no member surfaces starts no roster query, and
  // one that starts none shows nothing that would be empty (@Cindy, #task:2039
  // review — capability, not "the response happened to be empty").
  const rosterTarget = rosterQueryTarget(paneCaps, conversation.id, gate.allowMemberEffects);

  // Per-conversation view tabs (#task:2121). The strip shows only on the primary
  // pane (`viewTabsEnabled`), behind the same member gate the header Tasks entry
  // used (a non-member browsing a public channel still sees only Chat), AND only
  // when this position forms a real Tasks-board container. The controller/hook
  // run unconditionally (React) — the gates only decide rendering.
  const tabController = useConversationTabs(conversation.id);
  // Derived from the SAME placement projection every other affordance reads
  // (`paneMode`), not re-sniffed from `workspace_id`. FAIL-CLOSED: a workspace
  // channel with no real workspace id (old/corrupt carrier) or an unsupported
  // placement yields `null`. That `null` is the SOLE gate for BOTH the board
  // mount and the Tasks affordance — see `viewTabsShown` — so a position that
  // cannot form a container shows no Tasks tab at all rather than a selectable
  // tab that reveals Chat beneath it (Cindy review).
  const taskBoardContainer = useMemo<TaskBoardContainer | null>(
    () =>
      taskBoardContainerForPane(paneMode, {
        conversationId: conversation.id,
      }),
    [paneMode, conversation.id],
  );
  const viewTabsShown = viewTabsEnabled && gate.allowMemberEffects && taskBoardContainer !== null;
  const tasksTabActive = viewTabsShown && tabController.activeKey === "tasks";
  // Chat owns the surface when there is no tab strip at all, or its tab is the
  // front one. A front Tasks/attachment tab is an opaque overlay over the still-
  // mounted Chat, so Chat's out-of-flow floating affordances (scroll-to-bottom
  // pill, inline find bar) must go dark while it's covered — they sit above the
  // overlay's z-index otherwise. Gating render (not state) keeps scroll + find
  // intact for the round-trip back.
  const chatSurfaceActive = !viewTabsShown || tabController.activeKey === "chat";
  // The attachment previewer tab currently in front, if any (#task:2122). Same
  // gate as the Tasks overlay — no tab strip, no attachment tabs. Resolved from
  // the ordered tabs so a removed/stale active key can't mount a ghost preview.
  const activeAttachmentTab = useMemo(() => {
    if (!viewTabsShown) return null;
    const active = tabController.tabs.find((t) => tabKey(t) === tabController.activeKey);
    return active && active.kind === "attachment" ? active : null;
  }, [viewTabsShown, tabController.tabs, tabController.activeKey]);
  // The panel surfaces this pane can host, each behind the SAME gate its
  // side-panel entry uses: a jump handler for Attachments (the locate
  // action is a row's whole point), a thread opener for Threads. ONE list feeds both the `＋` producer and the mount below, so a
  // surface this pane can't host is never offered.
  const hostablePanels = useMemo<readonly PanelTabKind[]>(() => {
    const out: PanelTabKind[] = [];
    if (onJumpToMessage) out.push("attachments");
    if (onOpenThread) out.push("threads");
    return out;
  }, [onJumpToMessage, onOpenThread]);
  // Already-open surfaces leave the menu: the strip is showing that tab, so
  // offering it again would be a dead entry. Both open → no `＋` at all.
  const addablePanels = useMemo(
    () =>
      hostablePanels.filter(
        (panel) => !tabController.tabs.some((t) => t.kind === "panel" && t.panel === panel),
      ),
    [hostablePanels, tabController.tabs],
  );
  // The panel tab in front, by IDENTITY — deliberately NOT filtered by
  // `hostablePanels`. A tab whose gate stopped holding (membership left, or not
  // yet resolved) keeps its selection and keeps the pane covered by the opaque
  // surface below; filtering here would drop the overlay and expose the Chat
  // stream under a tab chip that still reads as selected.
  const activePanelTab = useMemo(() => {
    if (!viewTabsShown) return null;
    const active = tabController.tabs.find((t) => tabKey(t) === tabController.activeKey);
    return active && active.kind === "panel" ? active.panel : null;
  }, [viewTabsShown, tabController.tabs, tabController.activeKey]);
  const activePanelHostable = activePanelTab !== null && hostablePanels.includes(activePanelTab);
  const handleAddPanel = useCallback(
    (panel: PanelTabKind) => {
      tabController.openTab({ kind: "panel", panel });
    },
    [tabController.openTab],
  );
  // Open (or focus) a md/html attachment's previewer tab. Only handed to the
  // message list when the tab strip is shown — otherwise md/html keep the
  // open-with-OS behaviour every other attachment type has.
  const handleOpenAttachment = useCallback(
    (attachment: AttachmentSnapshot) => {
      tabController.openTab({
        kind: "attachment",
        attachmentId: attachment.id,
        filename: attachment.filename,
        contentType: attachment.content_type,
      });
    },
    [tabController.openTab],
  );

  // Consume a sibling pane's one-shot request to open an attachment here
  // (#task:2122): the Attachments list can't reach this pane's tab controller, so
  // it relays the file up through App. `resolveAttachmentOpenRoute` decides the
  // destination against the conversation MOUNTED NOW versus the one the request
  // was issued from — a request from A that lands after a switch to B resolves to
  // `reject`, so we clear it WITHOUT acting (never route A's file into B's tabs,
  // never OS-open A's file on B; the OS-open fallback is only for the matching
  // source). Where the source matches it's the same rule the message card makes:
  // previewable md/html + a hosting tab strip → previewer tab; else open-with-OS
  // (its own toast on failure). Clearing is signalled back so a re-click re-fires.
  useEffect(() => {
    const req = openAttachmentRequest;
    if (!req) return;
    const att = req.attachment;
    const route = resolveAttachmentOpenRoute({
      requestConversationId: req.conversationId,
      currentConversationId: conversation.id,
      previewKind: attachmentPreviewKind(att.content_type, att.filename),
      tabStripShown: viewTabsShown,
    });
    if (route === "preview") {
      handleOpenAttachment(att);
    } else if (route === "os-open") {
      void openAttachmentExternally(att);
    }
    onAttachmentRequestConsumed?.();
  }, [
    openAttachmentRequest,
    conversation.id,
    viewTabsShown,
    handleOpenAttachment,
    onAttachmentRequestConsumed,
  ]);

  // Per-viewer server-scored unread for this conversation's reply-clusters
  // (#task:1226). The SINGLE source of the chip's "N new" + first-unread —
  // sparse (only roots with unread > 0). Settled from REST after
  // `unread_updated` or this client's successful observe(); the message-window
  // summary no longer carries unread. Gated OFF for a non-member (reading
  // per-viewer unread state is a member effect) — the hook disables its query +
  // WS interest when passed a null id.
  const conversationUnread = useConversationUnread(
    gate.allowMemberEffects ? conversation.id : null,
  ).data;
  const threadUnreadByRoot = conversationUnread?.threads;
  // #task:1773: header attention dot — lit while any FOLLOWED thread in this
  // channel is unread (`followed ∩ unread`). Server-owned boolean carried on the
  // same `/unread` response, so it lights on reply / clears on observe with zero
  // new wiring. This flag and the panel's filtered Unread list are two
  // independent reads over one predicate → eventual convergence, so we render it
  // verbatim (never derive it from a list, never suppress it on a momentarily
  // empty list). A false-by-default (missing key) reads as "no attention".
  const hasFollowedThreadUnread = conversationUnread?.followed_thread_unread ?? false;

  const { data: agents = [] } = useAgents();
  // Single batch fetch for every agent avatar in this channel — threaded
  // down to MemberStrip and MessageList (→ MessageItem authors) so the
  // same blob URL is reused everywhere instead of independent hook
  // instances each firing the same network calls. The Composer's
  // activity header (variant A) sources run-state on its own.
  const agentAvatarUrls = useAgentAvatarObjectUrls(agents);

  const closed = conversation.closed_at !== null;
  const isArchived = conversation.archived_at !== null;
  const canOpenSettings = hasChannelNamespace && gate.allowMutations;
  // Visibility toggle + self-leave are workspace/open-roster only. A
  // fixed-roster conversation has neither a visibility choice nor a leave path.
  // The header quick-add requires an open, mutable roster and workspace candidates.
  const addMemberWorkspaceId =
    canAddMember(conversation, closed) && gate.allowMutations ? paneCaps.workspaceScope : null;
  const nav = useNav();
  const route = useRouteState();
  const settingsActive = route.channelSettingsId === conversation.id;
  // The header roster/agent strip reads only member-visible channels.
  // `rosterTarget` is null for a non-member and for a placement with no roster
  // (a `direct_pair`), and a null id disables the query AND the WS interest — so
  // a DM starts neither read.
  const { data: channelAgents = [] } = useChannelAgents(rosterTarget);
  // NOT defaulted: the run-state consumers distinguish "the read has not
  // settled" from "settled, this agent has no slot row", and `= []` collapses
  // the two into a confident Idle for every member (#task:2123).
  const { data: channelAgentRuntime } = useChannelAgentRuntime(rosterTarget);
  // The roster/agent strip is a member surface — hidden for a
  // non-member (channelAgents is also empty there, the fetch being gated off) —
  // and a ROSTER surface: a pair's two endpoints are not a roster, so the mode
  // withholds it outright (shiki, #task:2039).
  const compact = useCompactLayout();
  const memberStrip =
    !compact && rosterTarget !== null ? (
      <MemberStrip
        channelId={conversation.id}
        agentRuntime={channelAgentRuntime}
        agentAvatarUrls={agentAvatarUrls}
        onOpenMembers={() => nav.openChannelSettings(conversation.id)}
      />
    ) : null;

  // Thread children carry a single thread_anchor system message — the
  // header surfaces it as "Thread — #parent" + jump-back. The row
  // itself is filtered out of the message list below.
  const messageWindow = useChannelMessageWindow({
    conversationId: conversation.id,
    jumpTarget,
    jumpPrimePending,
    onJumped,
    onThreadAnchorResolved,
    markReadEnabled: gate.allowMemberEffects,
  });
  const { anchor, anchorMessageId, visibleMessages, effectiveJumpMessageId } = messageWindow;
  const showThreadHeader = !!onCloseChild && !!anchor;

  // A message jump's destination is the chat stream, so any jump landing in THIS
  // conversation brings Chat to the front. Without it the scroll/highlight
  // happens under whatever tab is in front (an opaque overlay) and the viewer
  // sees nothing — true for the Attachments tab's own locate action and for an
  // external jump (Saved, Activity, a task) that happens to target the open
  // conversation. One destination rule, not a second reveal state.
  useEffect(() => {
    if (effectiveJumpMessageId === null) return;
    tabController.setActive("chat");
  }, [effectiveJumpMessageId, tabController.setActive]);

  // The reply affordance (open thread + per-root reply badge) is
  // actionable only on the parent pane, gated by `onOpenThread` being
  // passed. The secondary `?side` pane passes it undefined
  // so the action is hidden. Per-root reply counts come from the message
  // window's cut3-6 sidecar (`replyCountsByRoot`) — there is no separate
  // threads query anymore.

  // Bind a TaskRefContext to this pane's conversation. Inline `#task:<n>`
  // chips inside this pane resolve their numeric reference against
  // `conversation.id` — the message's conversation, not the primary-selected
  // one. The server derives that conversation's board (workspace OR DirectPair)
  // and point-reads by number, so cross-workspace `?side` panes and DM panes
  // both resolve transparently; the click hands up the resolved id, which
  // needs no scope. `workspace_id` (null for a DM) is the CACHE-scope coord —
  // it collapses this channel and its siblings onto one by-number cell.
  const taskRefContext = useMemo(
    () => ({
      conversationId: conversation.id,
      workspaceId: conversation.workspace_id,
      onOpenTask,
    }),
    [conversation.id, conversation.workspace_id, onOpenTask],
  );
  const handleOpenChannelRef = useCallback(
    (channelId: string) => {
      // `#channel` refs resolve per-workspace, so a workspace-less pane has no
      // namespace to open one in and the affordance is withheld below rather
      // than guessing a workspace here.
      const workspaceId = paneCaps.workspaceScope;
      if (workspaceId === null) return;
      onOpenChannelRef?.(workspaceId, channelId);
    },
    [paneCaps.workspaceScope, onOpenChannelRef],
  );

  // In-conversation header find (#task:1335). Local to this pane. Navigation is
  // an "ensure the match is visible" op, NOT "reopen a message window" (Cindy):
  //   • match already in the LOADED window (data, not DOM — a virtualized row
  //     can be loaded-but-unmounted) → MessageList scrolls to it locally via
  //     `searchActiveMessageId` (no prime, no list remount → no white flash).
  //   • match off the loaded window → App's jump/prime path materialises it.
  // Offered only when the pane supports being jumped into (`onJumpToMessage`
  // present) — secondary `?side` panes don't, so they show no find affordance.
  const findEnabled = !!onJumpToMessage;
  // In-channel find is a member surface — the allowlist restores it
  // only on join. Gate the toggle, the ⌘F hotkey, and the overlay together.
  const findAvailable = findEnabled && gate.allowMemberEffects;
  const loadedMessageIds = useMemo(
    () => new Set(visibleMessages.map((m) => m.id)),
    [visibleMessages],
  );
  const inlineFindNavigate = useCallback(
    (messageId: string) => {
      // Ensure-visible: an in-window match needs no prime — the reactive local
      // scroll in MessageList (keyed on the active match id) positions it
      // without touching the message window. Only escalate genuine off-window
      // matches to the heavier jump/prime path.
      if (loadedMessageIds.has(messageId)) return;
      onJumpToMessage?.({
        workspaceId: conversation.workspace_id ?? undefined,
        channelId: conversation.id,
        messageId,
        // Main-stream ensure-visible: prime + locate in THIS already-open
        // channel without a route change, so an open thread side panel isn't
        // collapsed (shiki/Cindy #task:1335).
        preserveSidePanel: true,
      });
    },
    [loadedMessageIds, onJumpToMessage, conversation.workspace_id, conversation.id],
  );
  const find = useInlineFindController({
    conversationId: conversation.id,
    stream: "mainline",
    onNavigate: inlineFindNavigate,
    // Gate the QUERY SOURCE, not just the toggle: a non-member must never reach
    // the member-only /api/search, and a downgrade while open force-clears it.
    enabled: findAvailable,
  });
  // ⌘F opens the find. Channel is the primary pane, so it also claims ⌘F when
  // focus is nowhere specific (see useInlineFindHotkey).
  const findRootRef = useRef<HTMLElement>(null);
  // Scopes the text-selection Quote/Copy toolbar to this channel's message
  // list (#task:1544) so a selection routes to the main composer.
  const messageAreaRef = useRef<HTMLDivElement>(null);
  useInlineFindHotkey({
    rootRef: findRootRef,
    enabled: findAvailable,
    isPrimary: true,
    onOpen: find.openFind,
  });

  // Compact iOS draws this header natively; the page keeps every state and
  // handler, and the web header and tab strip step aside.
  const nativeBarAvailable = useNativeBarPlatform();
  const nativeTitle = showThreadHeader ? (parentTitle ?? conversation.title) : conversation.title;
  const counterpart = paneMode.kind === "direct_message" ? paneMode.counterpart : null;
  const exportHeader = useMemo(
    () => ({ title: conversation.title, counterpart }),
    [conversation.title, counterpart],
  );
  const nativeTitleIcon = useHeaderIdentityIcon(paneCaps.headerIdentity, counterpart);
  // A pair's title opens its counterpart, as the web header's face does.
  const openCounterpart =
    counterpart?.status === "active"
      ? counterpart.kind === "agent"
        ? {
            hint: t("conversationShell.opensActivity"),
            run: () => {
              focusAgentSideSection(counterpart.id, "activity");
              nav.openAgentPanel(counterpart.id);
            },
          }
        : {
            hint: t("conversationShell.opensProfile"),
            run: () => nav.openHumanPanel(counterpart.id),
          }
      : null;
  const titleAction = showThreadHeader
    ? null
    : canOpenSettings
      ? t("conversationShell.opensChannelSettings")
      : (openCounterpart?.hint ?? null);
  const locale = useLocale();
  const nativeBar = useMemo<PageBarDeclaration | null>(() => {
    if (!nativeBarAvailable) {
      return null;
    }

    const actions = [
      findAvailable
        ? {
            id: "find",
            label: t("common.search"),
            accessibilityLabel: t("conversationShell.searchThisChannel"),
            icon: iconMarkup(SearchIcon),
            selected: find.open,
          }
        : null,
      onToggleThreadsList && gate.allowMemberEffects
        ? {
            id: "threads",
            label: t("conversationShell.threads"),
            accessibilityLabel: hasFollowedThreadUnread
              ? t("conversationShell.threadsUnread")
              : undefined,
            icon: iconMarkup(ThreadIcon),
            selected: threadsListActive,
            dot: hasFollowedThreadUnread,
          }
        : null,
      onToggleFilesList && gate.allowMemberEffects
        ? {
            id: "files",
            label: t("conversationShell.attachments"),
            icon: iconMarkup(AttachmentIcon),
            selected: filesListActive,
          }
        : null,
      onToggleFullscreen
        ? {
            id: "fullscreen",
            label: isFullscreen
              ? t("conversationShell.exitFullScreen")
              : t("conversationShell.fullScreen"),
            icon: iconMarkup(isFullscreen ? CollapseIcon : ExpandIcon),
          }
        : null,
      onCloseChild ? { id: "close", label: closeLabel, icon: iconMarkup(CloseIcon) } : null,
    ].filter((action) => action !== null);

    return {
      title: nativeTitle,
      subtitle: isArchived
        ? t("common.archived")
        : closed
          ? t("conversationShell.closed")
          : showThreadHeader
            ? t("conversationShell.thread")
            : undefined,
      titleAction,
      titleIcon: titleAction ? nativeTitleIcon : undefined,
      actions: actions.length > 0 ? [actions] : [],
      overflow:
        actions.length > 1
          ? { label: t("conversationShell.more"), icon: iconMarkup(MoreIcon) }
          : undefined,
      tabs: viewTabsShown
        ? tabController.tabs.map((tab) => {
            const key = tabKey(tab);
            const { icon, label } = tabPresentation(tab);
            const unread =
              tab.kind === "panel" && tab.panel === "threads" && hasFollowedThreadUnread;
            return {
              id: key,
              label,
              accessibilityLabel: unread ? `${label} (unread)` : undefined,
              icon: iconMarkup(icon),
              selected: key === tabController.activeKey,
              dot: unread,
              closeLabel: isRemovable(tab) ? t("conversationShell.remove") : null,
            };
          })
        : null,
      addTabs:
        viewTabsShown && addablePanels.length > 0
          ? {
              label: t("conversationShell.addAView"),
              items: addablePanels.map((panel) => ({
                id: panel,
                label: panelPresentation()[panel].label,
                icon: iconMarkup(panelPresentation()[panel].icon),
              })),
            }
          : null,
    };
  }, [
    locale,
    nativeBarAvailable,
    nativeTitle,
    findAvailable,
    find.open,
    onToggleThreadsList,
    onToggleFilesList,
    onToggleFullscreen,
    onCloseChild,
    gate.allowMemberEffects,
    hasFollowedThreadUnread,
    threadsListActive,
    filesListActive,
    isFullscreen,
    closeLabel,
    isArchived,
    closed,
    showThreadHeader,
    titleAction,
    nativeTitleIcon,
    viewTabsShown,
    tabController.tabs,
    tabController.activeKey,
    addablePanels,
  ]);
  const native = useNativeBar(nativeBar, {
    title: () => {
      if (canOpenSettings) {
        nav.openChannelSettings(conversation.id);
      } else {
        openCounterpart?.run();
      }
    },
    action: ({ id }) => {
      if (id === "find") {
        if (find.open) {
          find.close();
        } else {
          find.openFind();
        }
      } else if (id === "threads") {
        onToggleThreadsList?.();
      } else if (id === "files") {
        onToggleFilesList?.();
      } else if (id === "fullscreen") {
        onToggleFullscreen?.();
      } else if (id === "close") {
        onCloseChild?.();
      }
    },
    tab: ({ id }) => tabController.setActive(id),
    closeTab: ({ id }) => tabController.closeTab(id),
    moveTab: ({ id, to }) => {
      const from = tabController.tabs.findIndex((tab) => tabKey(tab) === id);
      if (from >= 0 && to >= 0 && to < tabController.tabs.length) {
        tabController.reorder(from, to);
      }
    },
    addTab: ({ id }) => {
      if (id === "attachments" || id === "threads") {
        handleAddPanel(id);
      }
    },
  });
  const nativeHeader = native.active;

  return (
    <TaskRefContextProvider value={taskRefContext}>
      <ConversationPaneCapabilitiesProvider value={paneCaps}>
        <main
          ref={findRootRef}
          data-panel="chat"
          className="relative flex h-full min-w-0 flex-1 flex-col overflow-hidden"
          style={paneChrome}
        >
          <FloatingPaneChrome clearance="scroller">
            {nativeHeader ? (
              native.clearance
            ) : (
              <header
                data-conversation-header
                className="flex h-12 min-w-0 shrink-0 items-center gap-2 border-b border-border-subtle px-4"
              >
                <MobileNavigationSlot />
                {showThreadHeader ? (
                  <>
                    <ThreadIcon className="size-4 shrink-0 text-text-muted" />
                    <span
                      data-conversation-title
                      className="min-w-0 truncate font-serif text-base font-semibold text-text-primary"
                    >
                      {/* A thread is a reply-cluster, not a channel: the
                    `ThreadIcon` above is its identity glyph, so the
                    channel `#` (`[data-conversation-hash-icon]`) does NOT
                    belong here — only true channel/DM headers carry it
                    (shiki, #task:1373). */}
                      {parentTitle ?? conversation.title}
                    </span>
                    {memberStrip}
                    {!compact && addMemberWorkspaceId !== null ? (
                      <AddConversationMemberPopover
                        workspaceId={addMemberWorkspaceId}
                        conversationId={conversation.id}
                      />
                    ) : null}
                    {onJumpToOriginal && anchorMessageId ? (
                      <button
                        type="button"
                        onClick={() => onJumpToOriginal(anchorMessageId)}
                        className="text-xs text-accent hover:underline"
                        title={t("conversationShell.jumpToTheSource")}
                      >
                        {t("conversationShell.viewInChannel")}
                      </button>
                    ) : null}
                  </>
                ) : (
                  <>
                    {/* Identity by MODE, resolved in one place (#task:2039): a workspace
                  channel wears the `#` (shiki: "只要是 channel 都需要 # icon,逻辑上
                  不需要分支", #task:1373 — that ruling is about channels, and a
                  `direct_pair` is not one), a direct line wears its counterpart's
                  face. Threads take the branch above and wear the `ThreadIcon`. */}
                    {compact && canOpenSettings ? (
                      // Compact drops the settings button: the title opens settings,
                      // where members also live.
                      <button
                        type="button"
                        data-conversation-title-button
                        onClick={() => nav.openChannelSettings(conversation.id)}
                        aria-label={t("conversationShell.channelSettings")}
                        className="focus-outline flex min-w-0"
                      >
                        <ConversationHeaderIdentity
                          identity={paneCaps.headerIdentity}
                          title={conversation.title}
                          counterpart={null}
                        />
                      </button>
                    ) : (
                      <ConversationHeaderIdentity
                        identity={paneCaps.headerIdentity}
                        title={conversation.title}
                        counterpart={
                          paneMode.kind === "direct_message" ? paneMode.counterpart : null
                        }
                      />
                    )}
                    {memberStrip}
                    {!compact && addMemberWorkspaceId !== null ? (
                      <AddConversationMemberPopover
                        workspaceId={addMemberWorkspaceId}
                        conversationId={conversation.id}
                      />
                    ) : null}
                    {parentTitle ? (
                      <span className="flex min-w-0 items-center gap-1 truncate text-xs text-text-muted">
                        <ThreadIcon className="size-3 shrink-0 text-text-muted/60" />
                        <span className="truncate">
                          {t("conversationShell.threadOf", { title: parentTitle })}
                        </span>
                      </span>
                    ) : null}
                  </>
                )}
                {closed ? (
                  <span className="rounded-full bg-bg-elevated px-2 py-0.5 text-(length:--text-badge) uppercase tracking-wider text-text-muted">
                    {t("conversationShell.closed2")}
                  </span>
                ) : null}
                {isArchived ? (
                  <span className="rounded-full bg-accent-soft px-2 py-0.5 text-(length:--text-badge) uppercase tracking-wider text-accent">
                    {t("conversationShell.archived")}
                  </span>
                ) : null}

                <div
                  data-channel-actions
                  className={cn(
                    "ml-auto flex shrink-0 items-center gap-1.5",
                    compact && "glass-surface",
                  )}
                >
                  {findAvailable ? (
                    <button
                      type="button"
                      data-sticker
                      data-sticker-size="md"
                      data-sticker-tone="neutral"
                      onClick={find.open ? find.close : find.openFind}
                      aria-label={t("conversationShell.searchThisChannel")}
                      aria-pressed={find.open}
                      title={t("conversationShell.searchThisChannel")}
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
                  {onToggleThreadsList && gate.allowMemberEffects ? (
                    <button
                      type="button"
                      data-sticker
                      data-sticker-size="md"
                      data-sticker-tone="neutral"
                      onClick={onToggleThreadsList}
                      aria-label={
                        hasFollowedThreadUnread
                          ? t("conversationShell.threadsUnread")
                          : t("conversationShell.threads")
                      }
                      aria-pressed={threadsListActive}
                      title={t("conversationShell.threads")}
                      className={
                        "relative flex size-7 shrink-0 items-center justify-center rounded-(--control-radius) " +
                        (threadsListActive
                          ? "bg-bg-hover text-text-primary"
                          : "text-text-secondary hover:bg-bg-hover hover:text-text-primary")
                      }
                    >
                      <ThreadIcon className="size-3.5" />
                      {hasFollowedThreadUnread ? (
                        // #task:1773: ambient attention dot (same recipe as the NavRail
                        // unread dot). Decorative — the unread state is already in the
                        // aria-label — so pointer-events-none and aria-hidden.
                        <span
                          aria-hidden
                          className="pointer-events-none absolute right-1 top-1 size-1.5 rounded-full bg-(--unread)"
                        />
                      ) : null}
                    </button>
                  ) : null}
                  {onToggleFilesList && gate.allowMemberEffects ? (
                    <button
                      type="button"
                      data-sticker
                      data-sticker-size="md"
                      data-sticker-tone="neutral"
                      onClick={onToggleFilesList}
                      aria-label={t("conversationShell.attachments")}
                      aria-pressed={filesListActive}
                      title={t("conversationShell.attachments")}
                      className={
                        "flex size-7 shrink-0 items-center justify-center rounded-(--control-radius) " +
                        (filesListActive
                          ? "bg-bg-hover text-text-primary"
                          : "text-text-secondary hover:bg-bg-hover hover:text-text-primary")
                      }
                    >
                      <AttachmentIcon className="size-3.5" />
                    </button>
                  ) : null}
                  {canOpenSettings && !compact ? (
                    <button
                      type="button"
                      data-sticker
                      data-sticker-size="md"
                      data-sticker-tone="neutral"
                      onClick={() =>
                        settingsActive
                          ? nav.closeChannelSettings()
                          : nav.openChannelSettings(conversation.id)
                      }
                      aria-pressed={settingsActive}
                      aria-label={t("conversationShell.channelSettings")}
                      title={t("conversationShell.channelSettings")}
                      className={
                        "flex size-7 shrink-0 items-center justify-center rounded-(--control-radius) " +
                        (settingsActive
                          ? "bg-bg-hover text-text-primary"
                          : "text-text-secondary hover:bg-bg-hover hover:text-text-primary")
                      }
                    >
                      <SettingsIcon className="size-3.5" />
                    </button>
                  ) : null}
                  {onToggleFullscreen ? (
                    <button
                      type="button"
                      data-sticker
                      data-sticker-size="md"
                      data-sticker-tone="neutral"
                      onClick={onToggleFullscreen}
                      aria-label={
                        isFullscreen
                          ? t("conversationShell.exitFullScreen")
                          : t("conversationShell.fullScreen")
                      }
                      title={
                        isFullscreen
                          ? t("conversationShell.exitFullScreen")
                          : t("conversationShell.fullScreen")
                      }
                      className="flex size-7 shrink-0 items-center justify-center rounded text-text-secondary hover:bg-bg-hover hover:text-text-primary"
                    >
                      {isFullscreen ? (
                        <CollapseIcon className="size-3.5" />
                      ) : (
                        <ExpandIcon className="size-3.5" />
                      )}
                    </button>
                  ) : null}
                  {onCloseChild ? (
                    <button
                      type="button"
                      data-sticker
                      data-sticker-size="md"
                      data-sticker-tone="neutral"
                      onClick={onCloseChild}
                      aria-label={closeLabel}
                      title={closeLabel}
                      className="flex size-7 shrink-0 items-center justify-center rounded text-text-secondary hover:bg-bg-hover hover:text-text-primary"
                    >
                      <CloseIcon className="size-3.5" />
                    </button>
                  ) : null}
                </div>
              </header>
            )}

            {/* Per-conversation view tabs (#task:2121) — a flex sibling under the
          header. When shown it adds the tab strip, so the find-bar overlay below
          drops from `top-12` to clear the h-12 header PLUS the strip. Strip
          height is the single `--conversation-tab-strip-height` authority the bar
          itself reads, so the two never drift. */}
            {viewTabsShown && !nativeHeader ? (
              <ConversationTabBar
                tabs={tabController.tabs}
                activeKey={tabController.activeKey}
                onActivate={tabController.setActive}
                onReorder={tabController.reorder}
                onClose={tabController.closeTab}
                addablePanels={addablePanels}
                onAddPanel={handleAddPanel}
                threadsUnread={hasFollowedThreadUnread}
              />
            ) : null}
          </FloatingPaneChrome>

          {/* Inline find bar (#task:1335) — a header OVERLAY, not a flex sibling of
          the message list. Anchored just below the h-12 header (`top-12`, or
          below the header + tab strip when it shows) and taken out of flow
          (`absolute`) so opening / closing / typing NEVER changes the scroll
          container's height → no list reflow / flicker (shiki: 搜索框不应挤压
          对话流、不应触发重渲染). It only drives the search controller + this
          overlay; the message window is untouched until the user navigates to a
          match (ensure-visible below). */}
          {findAvailable && find.open && chatSurfaceActive ? (
            <div
              data-find-overlay
              className={`absolute inset-x-0 z-(--z-sticky) ${
                viewTabsShown ? "top-[calc(3rem+var(--conversation-tab-strip-height))]" : "top-12"
              }`}
            >
              <InlineFindBar
                controller={find}
                onOpenAdvanced={() =>
                  onOpenAdvancedSearch?.({
                    query: find.query,
                    workspaceId: conversation.workspace_id ?? undefined,
                    // A pair has no workspace to hand over, so it hands over the
                    // counterpart instead — otherwise the advanced panel opened
                    // from a DM carried NO seed at all and kept whatever workspace
                    // filter the session had, which filtered the DM's own results
                    // away (#task:2114).
                    sender:
                      paneMode.kind === "direct_message"
                        ? directPairSenderSeed(paneMode.counterpart)
                        : undefined,
                  })
                }
                placeholder={t("conversationShell.searchThisChannel")}
              />
            </div>
          ) : null}

          <div className="relative flex min-h-0 min-w-0 flex-1 flex-col">
            {/* Message area + activity dock share a relative box so the dock
            can float out-of-flow at the bottom (above the composer). The
            scroller's viewport height never changes when the dock shows /
            hides / grows — no content judder. */}
            <div ref={messageAreaRef} className="relative flex min-h-0 min-w-0 flex-1 flex-col">
              <MessageList
                channelId={conversation.id}
                workspaceId={conversation.workspace_id}
                exportHeader={exportHeader}
                {...messageWindow.listProps}
                canReact={!closed && !isArchived && gate.allowMutations}
                canSave={gate.allowMutations}
                canParticipate={gate.allowMemberEffects}
                canMutateLinkedTask={!closed && !isArchived && gate.allowMutations}
                agentAvatarUrls={agentAvatarUrls}
                leadingSlot={
                  anchor ? (
                    <QuotedMessagePreview
                      quoteMessage={anchor}
                      onJumpToOriginal={
                        onJumpToOriginal && anchorMessageId
                          ? () => onJumpToOriginal(anchorMessageId)
                          : undefined
                      }
                    />
                  ) : null
                }
                highlightedMessageId={highlightedMessageId ?? null}
                onOpenThread={onOpenThread}
                threadUnreadByRoot={threadUnreadByRoot}
                conversationRefsById={conversationRefsById}
                onOpenConversationRef={onOpenConversationRef}
                onOpenAttachment={viewTabsShown ? handleOpenAttachment : undefined}
                onOpenChannelRef={
                  // A `#channel` chip resolves inside a workspace, so a
                  // workspace-less pane offers no navigation for one (it still
                  // renders as muted text, exactly like an unresolvable ref).
                  onOpenChannelRef && paneCaps.workspaceScope !== null
                    ? handleOpenChannelRef
                    : undefined
                }
                searchQuery={find.highlightQuery}
                searchActiveMessageId={find.activeMessageId}
                surfaceActive={chatSurfaceActive}
              />
              <SelectionQuoteToolbar boundaryRef={messageAreaRef} draftKey={conversation.id} />
            </div>

            {gate.showComposer ? (
              <Composer
                key={conversation.id}
                conversationId={conversation.id}
                directory={composerDir}
                disabled={closed}
                archived={isArchived}
                // A pair whose counterpart principal is gone: writes can never
                // succeed again, which is not the same fact as `closed` and must not
                // borrow its copy.
                readOnly={conversation.read_only === true}
                covered={!chatSurfaceActive}
              />
            ) : gate.showJoin ? (
              // Resolved non-member browsing a public channel: Join replaces the
              // composer. `unknown` (membership still resolving) shows NEITHER — a
              // neutral state — so we never flash a Join affordance we might revoke.
              <ChannelJoinBar
                onJoin={() => joinChannel.mutate(conversation.id)}
                joining={joinChannel.isPending}
                channelTitle={conversation.title}
              />
            ) : null}

            {/* Tasks tab — an opaque overlay over the chat column (message list +
            composer) rather than a mount swap, so Chat stays mounted and its
            scroll position survives a tab round-trip. Rendered only while the
            Tasks tab is active, a task-open handler exists (the primary pane),
            AND a board container resolved (fail-closed: a workspace channel with
            no real workspace id mounts nothing). A card opens detail in the side
            pane via the same `onOpenTask` an inline `#task` chip uses. `inset-0`
            covers the whole column. */}
            {tasksTabActive && taskBoardContainer && onOpenTask ? (
              <div
                data-conversation-overlay
                className="absolute inset-0 z-(--z-raised) bg-(--pane-bg,var(--bg-base))"
              >
                <ChannelTasksTab container={taskBoardContainer} onOpenTask={onOpenTask} />
              </div>
            ) : null}

            {/* Attachment previewer tab (#task:2122) — same overlay discipline as
            Tasks: Chat stays mounted underneath so its scroll survives a tab
            round-trip. Keyed on the attachment id so switching between two
            attachment tabs remounts the fetch/decode rather than reusing stale
            body state. */}
            {activeAttachmentTab ? (
              <div
                data-conversation-overlay
                className="absolute inset-0 z-(--z-raised) bg-(--pane-bg,var(--bg-base))"
              >
                <ChannelAttachmentTab
                  key={activeAttachmentTab.attachmentId}
                  attachmentId={activeAttachmentTab.attachmentId}
                  filename={activeAttachmentTab.filename}
                  contentType={activeAttachmentTab.contentType}
                />
              </div>
            ) : null}

            {/* Attachments / Threads as a tab — same overlay discipline as
            Tasks, so Chat keeps its scroll underneath. Keyed on the surface so
            switching between two panel tabs remounts the surface's own
            filter/scroll rather than carrying one list's state into another. */}
            {activePanelTab ? (
              <div
                data-conversation-overlay
                className="absolute inset-0 z-(--z-raised) bg-(--pane-bg,var(--bg-base))"
              >
                {activePanelHostable ? (
                  <ConversationPanelTab
                    key={activePanelTab}
                    panel={activePanelTab}
                    channelId={conversation.id}
                    workspaceId={conversation.workspace_id}
                    // Present by construction: `hostablePanels` only offers
                    // Attachments when this handler exists.
                    onJumpToMessage={(target) => onJumpToMessage?.(target)}
                    // Same pane, so an md/html row opens the previewer tab directly —
                    // no sibling relay (that exists only because the side panel can't
                    // reach this pane's tab controller). Images and other types stay
                    // the row's own business (lightbox / open-with-OS).
                    onOpenAttachment={handleOpenAttachment}
                    onOpenThread={onOpenThread ? (rootId) => onOpenThread(rootId) : undefined}
                  />
                ) : (
                  <div className="flex h-full items-center justify-center px-6 text-xs text-text-muted">
                    {t("conversationShell.thisViewIsUnavailable")}
                  </div>
                )}
              </div>
            ) : null}
          </div>
        </main>
      </ConversationPaneCapabilitiesProvider>
    </TaskRefContextProvider>
  );
}
