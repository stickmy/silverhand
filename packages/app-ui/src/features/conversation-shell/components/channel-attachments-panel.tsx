import { getPlatform } from "@/platform";
import { fetchAttachmentBytes } from "@/lib/api";
import { AttachmentsSkeleton } from "./channel-list-skeletons";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { paneChrome } from "@/components/layout/pane-chrome";
import dayjs from "dayjs";
import { GroupedVirtuoso } from "react-virtuoso";
import { toast } from "@/lib/toast";
import * as Select from "@radix-ui/react-select";
import {
  AttachmentIcon,
  ChevronDownIcon,
  ClockIcon,
  DocumentIcon,
  DownloadIcon,
  ImageIcon,
  MembersIcon,
  OpenExternalIcon,
  SpinnerIcon,
} from "@/icons";
import type {
  AgentDto,
  AttachmentDto,
  AttachmentListItem,
  AttachmentListItemSource,
  AttachmentSenderFacet,
  UserDto,
} from "@silverhand/shared";
import { AgentAvatar } from "@/components/agent-avatar";
import { UserAvatar } from "@/components/user-avatar";
import { AuthorAvatarView, AuthorName } from "@/features/messages/components/author-avatar-view";
import {
  presentAuthor,
  type AuthorPresentation,
} from "@/features/messages/model/author-presentation";
import { useAuthorAvatarObjectUrls } from "@/features/messages/hooks/author-avatar-hooks";
import { CollectionItemCard } from "@/components/layout/collection-item-card";
import { SidePanelHeader, SidePanelHeaderTitle } from "@/components/layout/side-panel-header";
import {
  INITIAL_ATTACHMENTS_WINDOW,
  reduceAttachmentsWindow,
} from "@/features/conversation-shell/model/attachments-window";
import { useMessageRenderContext } from "@/features/messages/hooks/use-message-render-context";
import { useAgents } from "@/features/agents/hooks/hooks";
import { useAgentAvatarObjectUrls } from "@/features/agents/hooks/avatar-hooks";
import {
  useChannelAttachments,
  useChannelAttachmentSenders,
  ALL_ATTACHMENTS_FILTER,
  isAttachmentFilterActive,
  type AttachmentListFilter,
  type AttachmentSenderFilter,
} from "@/features/channels/use-channel-attachments";
import {
  groupAttachmentsByDay,
  resolveSenderIdentity,
  type AttachmentSenderActor,
  type AttachmentSenderIdentity,
} from "@/features/channels/attachments-view-model";
import { DateFilterSelect, type DateFilterValue } from "@/components/ui/date-filter-select";
import {
  FilterSelectItem,
  FILTER_SELECT_TRIGGER_CLASS,
  FILTER_SELECT_CONTENT_CLASS,
} from "@/components/ui/filter-select";
import { cn } from "@/lib/cn";
import { attachmentPreviewKind } from "@/features/attachments/attachment-preview";
import { saveAttachmentToDisk } from "@/features/messages/model/save-attachment";
import {
  formatOpenException,
  openAttachmentExternally,
} from "@/features/messages/model/open-attachment";
import { Lightbox } from "@/features/messages/components/lightbox";
import {
  getAttachmentBlobUrl,
  loadAttachmentBlobUrl,
  useAttachmentBlobToken,
} from "@/features/attachments/attachment-blob-cache";
import type { MessageJumpTarget } from "@/features/channels/use-message-jump-controller";
import { t } from "@/lib/i18n";

interface ChannelAttachmentsPanelProps {
  channelId: string;
  /** Null for a `direct_pair` (no workspace coordinate). The attachments read is
   *  channel-scoped; the workspace only scopes the sender-filter directory, which
   *  `useMessageRenderContext` already resolves to empty for a workspace-less
   *  conversation (#task:2096). */
  workspaceId: string | null;
  parentTitle?: string | null;
  onClose: () => void;
  /** Jump to the source message in the primary channel pane — reuses the
   *  same primitive as thread "View in channel" so a thread-scoped source
   *  lands in the right place. */
  onJumpToMessage: (target: MessageJumpTarget) => void;
  /** Open an md/html row in the primary pane's in-app previewer tab (#task:2122)
   *  rather than open-with-OS. The panel is a sibling pane with no reach into that
   *  pane's tab controller, so it hands the attachment up to App, which relays it.
   *  Only md/html route here (the panel decides via {@link attachmentPreviewKind});
   *  images open the local lightbox and other files still open-with-OS. */
  onOpenAttachment?: (attachment: AttachmentDto) => void;
  /** Injected in tests to pin day-boundary math to a fixed clock; production
   *  defaults to `Date.now()`. */
  nowMs?: number;
}

/**
 * Channel Attachments panel — the SIDE-PANEL host: `<aside>` +
 * `SidePanelHeader` around {@link ChannelAttachmentsContent}. The custom-tab
 * host composes the same content under its own container; the two
 * hosts share no shell.
 */
export function ChannelAttachmentsPanel({
  channelId,
  workspaceId,
  parentTitle,
  onClose,
  onJumpToMessage,
  onOpenAttachment,
  nowMs,
}: ChannelAttachmentsPanelProps): JSX.Element {
  return (
    <aside
      data-panel="side"
      data-pane="attachments-list"
      className="flex h-full min-h-0 w-full flex-col overflow-hidden"
      style={paneChrome}
    >
      <SidePanelHeader
        onClose={onClose}
        nativeBar={{
          title: t("conversationShell.attachments"),
          subtitle: parentTitle ? `#${parentTitle}` : undefined,
          titleAction: null,
          actions: [],
          tabs: null,
          addTabs: null,
        }}
      >
        <SidePanelHeaderTitle
          icon={<AttachmentIcon className="size-3.5 shrink-0 text-text-muted" />}
          title={t("conversationShell.attachments")}
          subtitle={parentTitle ? `#${parentTitle}` : ""}
        />
      </SidePanelHeader>
      {/* This host's inset. The pane-edge gutter belongs to the container, so
          the surface inside starts at x=0 and each host picks its own distance
          from the edge — a narrow side panel spends less of it than a tab. */}
      <div className="flex min-h-0 flex-1 flex-col px-1.5">
        <ChannelAttachmentsContent
          channelId={channelId}
          workspaceId={workspaceId}
          onJumpToMessage={onJumpToMessage}
          onOpenAttachment={onOpenAttachment}
          nowMs={nowMs}
        />
      </div>
    </aside>
  );
}

/**
 * The Attachments surface itself: every attachment referenced by a persisted
 * message in one channel, reverse-chronological, grouped by local day. Filter
 * bar, rows, empty/error states and its own virtualized scroller. Host-agnostic
 * — no title, no close — so the side panel and the conversation tab mount THIS
 * and can't drift on what an attachment row says.
 *
 * Utility surface — not a conversation, does not enter the visible/focused
 * compute for notification suppression.
 *
 * Rendered with `GroupedVirtuoso` so only viewport rows mount. This is a
 * hard requirement, not an optimization: without virtualization every row
 * would fire `loadAttachmentBlobUrl` on mount, and 40 image rows means 40
 * concurrent blob fetches of up to 10MB each — plus a per-row relative-time
 * interval that never unmounts as the list grows.
 *
 * Row identity is `attachment.id` via a React `key` on `AttachmentRow`
 * inside `itemContent`. Virtuoso may recycle a physical row-wrapper
 * position as pages append or realtime top-inserts land — when it does,
 * the changed key forces React to remount `AttachmentRow`, so its local
 * state (`lightboxOpen`, thumbnail `imageUrl` / `retriedRef` / `failed`)
 * never carries over from a different attachment. Virtuoso's own
 * `computeItemKey` is intentionally NOT used: in grouped mode it is fed
 * the physical index (including group headers), so a naive
 * `flatItems[index]` mapping would be off by one per preceding group
 * header. Keying the subtree is both simpler and correct.
 *
 * Width-adaptive: `conversation-surface` names the query container, so the
 * layout tracks THIS surface's own width (see `chrome.css`) — the narrow stack
 * of filename over byline becomes filename / author / size / date as aligned
 * columns once there is room. The surface fills whatever box its host gives it;
 * the outer inset belongs to the host, this component only spaces its own rows.
 */
export function ChannelAttachmentsContent({
  channelId,
  workspaceId,
  onJumpToMessage,
  onOpenAttachment,
  nowMs,
}: Omit<ChannelAttachmentsPanelProps, "onClose" | "parentTitle">): JSX.Element {
  const [filter, setFilter] = useState<AttachmentListFilter>(ALL_ATTACHMENTS_FILTER);
  // The sender facets are lazy: fetched only once the sender dropdown is
  // first opened, so a panel whose filter is never touched issues no facet
  // request. Once opened we keep it enabled (cheap, and keeps the option list
  // warm for reopens).
  const [senderMenuOpened, setSenderMenuOpened] = useState(false);

  const { items, sessionKey, isLoading, isError, isFetchingNextPage, hasNextPage, fetchNextPage } =
    useChannelAttachments(channelId, filter, { nowMs });

  // The directory maps drive only the sender-FILTER facet options (an
  // independent axis — an arbitrary distinct-senders list resolved by id, not
  // the rows' own authors).
  const { agentsById, humansById, humanAvatarUrls } = useMessageRenderContext(
    workspaceId,
    channelId,
  );
  const { data: agents = [] } = useAgents();
  const agentAvatarUrls = useAgentAvatarObjectUrls(agents);

  const {
    senders: senderFacets,
    isLoading: sendersLoading,
    isError: sendersError,
    refetch: refetchSenders,
  } = useChannelAttachmentSenders(channelId, {
    enabled: senderMenuOpened,
  });

  // Pin day-boundary math for the whole render pass so a row assigned to
  // "Today" doesn't slip to "Yesterday" mid-scroll if the wall clock ticks
  // past midnight. Recomputed on `items` change (a new page could add a row
  // in a new day) but not on every unrelated re-render.
  const groups = useMemo(
    () => groupAttachmentsByDay(items, { now: nowMs ?? Date.now() }),
    [items, nowMs],
  );
  const groupCounts = useMemo(() => groups.map((g) => g.items.length), [groups]);
  const flatItems = useMemo(() => groups.flatMap((g) => g.items), [groups]);

  // Each row's byline avatar bytes come STRICTLY from its own source author DTO
  // (#task:1792 B2), never a directory join — so an attachment whose author is
  // outside the loaded directory (or deleted) still renders its own avatar.
  const sourceAuthors = useMemo(() => flatItems.map((i) => i.source.author), [flatItems]);
  const { agentAvatarUrls: authorAgentAvatarUrls, humanAvatarUrls: authorHumanAvatarUrls } =
    useAuthorAvatarObjectUrls(sourceAuthors);

  // Keep the reader's scroll stable across two DISTINCT axes without conflating
  // them (see `reduceAttachmentsWindow`): a query-session change (different
  // dataset — reset baseline + remount, even when a cache hit skips the empty
  // window and the IDs overlap) vs an in-session head shift (realtime top-insert
  // or head-trim — preserve position by moving `firstItemIndex`). The reducer is
  // the single decision point; the effect only feeds it the observed inputs.
  const [attachWindow, setAttachWindow] = useState(INITIAL_ATTACHMENTS_WINDOW);
  useLayoutEffect(() => {
    const keys = flatItems.map((i) => i.attachment.id);
    setAttachWindow((prev) => reduceAttachmentsWindow(prev, { sessionKey, keys }));
  }, [flatItems, sessionKey]);

  const filterActive = isAttachmentFilterActive(filter);
  // Show the filter bar whenever there's something to filter or a filter is
  // already narrowing the view (so the user can widen it back). Hidden in the
  // pristine "no attachments yet" / loading / error states — no point offering
  // filters over an empty channel.
  const showFilterBar = !isLoading && !isError && (flatItems.length > 0 || filterActive);

  return (
    <div className="conversation-surface flex min-h-0 w-full flex-1 flex-col">
      {showFilterBar ? (
        <div data-attachments-filter-bar className="shrink-0">
          <div className="flex flex-wrap items-center gap-2 py-2">
            <SenderFilterSelect
              value={filter.sender}
              facets={senderFacets}
              isLoading={sendersLoading}
              isError={sendersError}
              agentsById={agentsById}
              humansById={humansById}
              agentAvatarUrls={agentAvatarUrls}
              humanAvatarUrls={humanAvatarUrls}
              onOpenChange={(open) => {
                if (open) {
                  setSenderMenuOpened(true);
                  // A prior failure leaves a false-empty dropdown; reopening
                  // retries rather than staying stuck on "no senders".
                  if (sendersError) refetchSenders();
                }
              }}
              onChange={(sender) => setFilter((f) => ({ ...f, sender }))}
            />
            <DateFilterSelect
              value={filter.time}
              ariaLabel={t("conversationShell.filterAttachmentsByTime")}
              triggerClassName="w-auto"
              onChange={(time: DateFilterValue) => setFilter((f) => ({ ...f, time }))}
            />
          </div>
        </div>
      ) : null}

      <div className="min-h-0 flex-1">
        {isLoading ? (
          <AttachmentsSkeleton />
        ) : isError ? (
          <div className="flex h-full flex-col items-center justify-center gap-1 px-6 text-center">
            <p className="text-sm text-text-primary">
              {t("conversationShell.couldntLoadAttachments")}
            </p>
            <p className="text-xs text-text-muted">
              {t("conversationShell.theAttachmentsIndexIsnt")}
            </p>
          </div>
        ) : flatItems.length === 0 ? (
          filterActive ? (
            <div className="flex h-full flex-col items-center justify-center gap-1 px-6 text-center">
              <AttachmentIcon className="size-6 text-text-muted/50" />
              <p className="text-sm text-text-primary">
                {t("conversationShell.noMatchingAttachments")}
              </p>
              <p className="text-xs text-text-muted">{t("conversationShell.noFilesMatchThe")}</p>
              <button
                type="button"
                onClick={() => setFilter(ALL_ATTACHMENTS_FILTER)}
                className="mt-1 rounded-(--control-radius) px-2 py-1 text-xs text-text-secondary hover:bg-bg-hover hover:text-text-primary"
              >
                {t("conversationShell.clearFilters")}
              </button>
            </div>
          ) : (
            <div className="flex h-full flex-col items-center justify-center gap-1 px-6 text-center">
              <AttachmentIcon className="size-6 text-text-muted/50" />
              <p className="text-sm text-text-primary">{t("conversationShell.noAttachmentsYet")}</p>
              <p className="text-xs text-text-muted">{t("conversationShell.filesSentInThis")}</p>
            </div>
          )
        ) : (
          <GroupedVirtuoso
            key={attachWindow.mountEpoch}
            style={{ height: "100%" }}
            firstItemIndex={attachWindow.firstItemIndex}
            groupCounts={groupCounts}
            groupContent={(index) => (
              <div className="bg-(--pane-bg,var(--bg-base))">
                <div className="pb-1 pt-2 text-(length:--text-badge) font-medium uppercase tracking-wide text-text-muted">
                  {groups[index]?.label ?? ""}
                </div>
              </div>
            )}
            itemContent={(index) => {
              // `firstItemIndex` shifts the ITEM index Virtuoso passes here into
              // absolute space (it starts at FIRST_ITEM_INDEX_START and moves as
              // rows prepend/head-trim), so map back to the local `flatItems`
              // index. Group indexes are NOT shifted, so `groupContent` above
              // still reads `groups[index]` directly. Missing the offset made
              // every row resolve to `undefined` → blank list.
              const item = flatItems[index - attachWindow.firstItemIndex];
              if (!item) return null;
              // Author identity via the ONE shared seam (#task:1792): the
              // source's server-authoritative `MessageAuthorDto` → `presentAuthor`,
              // never a live `from_*_id` directory re-join. Deleted → snapshot
              // name + neutral avatar; unresolved → "Unknown …"; a system-authored
              // attachment source (`author === null`) reads "System". (The sender
              // FILTER facet below is a separate axis — a distinct-senders list
              // with no per-row author — so it keeps its own resolver.)
              const presentation = item.source.author ? presentAuthor(item.source.author) : null;
              return (
                <div className="py-0.75">
                  <AttachmentRow
                    key={item.attachment.id}
                    item={item}
                    presentation={presentation}
                    authorAgentAvatarUrls={authorAgentAvatarUrls}
                    authorHumanAvatarUrls={authorHumanAvatarUrls}
                    onOpenAttachment={onOpenAttachment}
                    onJump={() =>
                      onJumpToMessage({
                        workspaceId: workspaceId ?? undefined,
                        channelId,
                        threadRootId: item.source.thread_root_id,
                        messageId: item.source.message_id,
                      })
                    }
                  />
                </div>
              );
            }}
            endReached={() => {
              if (hasNextPage) fetchNextPage();
            }}
            components={{
              Footer: () =>
                hasNextPage || isFetchingNextPage ? (
                  <div className="flex items-center justify-center py-3 text-text-muted">
                    <SpinnerIcon className="size-4 animate-spin" />
                  </div>
                ) : null,
            }}
          />
        )}
      </div>
    </div>
  );
}

// ── Sender avatar plumbing ───────────────────────────────────────────────

interface AvatarProps {
  kind: AttachmentListItemSource["from_kind"];
  runtime: string | null;
  avatarObjectUrl: string | null;
  label: string;
}

/** Compose the avatar inputs from the pure identity + resolver maps. Kept
 *  next to the panel because it's a pure lookup over UI-scope maps, not a
 *  reusable domain rule. */
function pickAvatarProps(
  identity: AttachmentSenderIdentity,
  agentsById: Map<string, AgentDto>,
  agentAvatarUrls: Map<string, string | null>,
  humanAvatarUrls: Map<string, string | null>,
): AvatarProps {
  const label = identity.label ?? t("conversationShell.system");
  if (identity.kind === "agent" && identity.agentId) {
    const a = agentsById.get(identity.agentId);
    return {
      kind: "agent",
      runtime: a?.runtime ?? null,
      avatarObjectUrl: agentAvatarUrls.get(identity.agentId) ?? null,
      label,
    };
  }
  if (identity.kind === "user" && identity.userId) {
    return {
      kind: "user",
      runtime: null,
      avatarObjectUrl: humanAvatarUrls.get(identity.userId) ?? null,
      label,
    };
  }
  return { kind: identity.kind, runtime: null, avatarObjectUrl: null, label };
}

function SenderAvatar({ avatar, size }: { avatar: AvatarProps; size: number }): JSX.Element {
  if (avatar.kind === "agent" && avatar.runtime) {
    return (
      <AgentAvatar
        runtime={avatar.runtime as AgentDto["runtime"]}
        size={size}
        avatarObjectUrl={avatar.avatarObjectUrl}
        alt={avatar.label}
        title={avatar.label}
      />
    );
  }
  return (
    <UserAvatar
      size={size}
      avatarObjectUrl={avatar.avatarObjectUrl}
      alt={avatar.label}
      title={avatar.label}
    />
  );
}

// ── Sender filter dropdown ───────────────────────────────────────────────
//
// A value-list Select over the channel's DISTINCT canonical-source senders
// (the facets). Every option is guaranteed wire-expressible by the list
// filter, so selecting one can never yield the pagination-window "0 rows"
// illusion. Identity (label + avatar) resolves through the SAME member maps
// the rows use, so a sender reads identically in the dropdown and the list.

/** Encode a sender filter as the Select's string value. */
function senderFilterToValue(s: AttachmentSenderFilter | null): string {
  if (!s) return "all";
  if (s.kind === "user") return `u:${s.userId}`;
  if (s.kind === "agent") return `a:${s.agentId}`;
  return "sys";
}

/** Decode a Select value back to a sender filter (`null` = all senders). */
function valueToSenderFilter(v: string): AttachmentSenderFilter | null {
  if (v === "sys") return { kind: "system" };
  if (v.startsWith("u:")) return { kind: "user", userId: v.slice(2) };
  if (v.startsWith("a:")) return { kind: "agent", agentId: v.slice(2) };
  return null;
}

/** The Select value for a facet — the same encoding as a selected filter. */
function facetValue(f: AttachmentSenderFacet): string {
  if (f.from_kind === "user") return `u:${f.from_user_id}`;
  if (f.from_kind === "agent") return `a:${f.from_agent_id}`;
  return "sys";
}

/** Reconstruct the sender-actor axis from a filter so the trigger can resolve
 *  the selected identity WITHOUT waiting on the facet list to (re)load. */
function actorFromFilter(s: AttachmentSenderFilter): AttachmentSenderActor {
  if (s.kind === "user") {
    return { from_kind: "user", from_user_id: s.userId, from_agent_id: null };
  }
  if (s.kind === "agent") {
    return { from_kind: "agent", from_user_id: null, from_agent_id: s.agentId };
  }
  return { from_kind: "system", from_user_id: null, from_agent_id: null };
}

function SenderFilterSelect({
  value,
  facets,
  isLoading,
  isError,
  agentsById,
  humansById,
  agentAvatarUrls,
  humanAvatarUrls,
  onChange,
  onOpenChange,
}: {
  value: AttachmentSenderFilter | null;
  facets: AttachmentSenderFacet[];
  isLoading: boolean;
  isError: boolean;
  agentsById: Map<string, AgentDto>;
  humansById: Map<string, UserDto>;
  agentAvatarUrls: Map<string, string | null>;
  humanAvatarUrls: Map<string, string | null>;
  onChange: (sender: AttachmentSenderFilter | null) => void;
  onOpenChange: (open: boolean) => void;
}): JSX.Element {
  const maps = { agentsById, humansById };
  const selectedIdentity = value ? resolveSenderIdentity(actorFromFilter(value), maps) : null;
  const selectedLabel =
    value == null
      ? t("conversationShell.allSenders")
      : (selectedIdentity?.label ?? t("conversationShell.system"));

  return (
    <Select.Root
      value={senderFilterToValue(value)}
      onValueChange={(v) => onChange(valueToSenderFilter(v))}
      onOpenChange={onOpenChange}
    >
      <Select.Trigger
        aria-label={t("conversationShell.filterAttachmentsBySender")}
        className={cn(FILTER_SELECT_TRIGGER_CLASS, "w-auto max-w-48")}
      >
        <span className="flex min-w-0 items-center gap-1.5">
          {value == null || selectedIdentity == null ? (
            <MembersIcon className="size-3.5 text-text-muted" />
          ) : (
            <span className="flex shrink-0">
              <SenderAvatar
                avatar={pickAvatarProps(
                  selectedIdentity,
                  agentsById,
                  agentAvatarUrls,
                  humanAvatarUrls,
                )}
                size={16}
              />
            </span>
          )}
          <span className="truncate">{selectedLabel}</span>
        </span>
        <Select.Icon>
          <ChevronDownIcon className="size-3.5 text-text-muted" />
        </Select.Icon>
      </Select.Trigger>
      <Select.Portal>
        <Select.Content
          position="popper"
          sideOffset={4}
          data-dropdown-frame
          className={FILTER_SELECT_CONTENT_CLASS}
        >
          <Select.Viewport className="p-1">
            <FilterSelectItem value="all" icon={<MembersIcon className="size-3.5" />}>
              {t("conversationShell.allSenders")}
            </FilterSelectItem>
            {isLoading && facets.length === 0 ? (
              // Distinct from a real empty set — a lazy fetch in flight must
              // not read as "this channel has no senders".
              <div
                role="status"
                className="flex items-center gap-2 px-2 py-1.5 text-xs text-text-muted"
              >
                <SpinnerIcon aria-hidden className="size-3 motion-safe:animate-spin" />
                {t("conversationShell.loadingSenders")}
              </div>
            ) : isError && facets.length === 0 ? (
              // Explicit failure, not false-empty; reopening the dropdown
              // retries (see onOpenChange).
              <div className="px-2 py-1.5 text-xs text-text-muted">
                {t("conversationShell.couldntLoadSenders")}
              </div>
            ) : (
              facets.map((facet) => {
                const identity = resolveSenderIdentity(facet, maps);
                const avatar = pickAvatarProps(
                  identity,
                  agentsById,
                  agentAvatarUrls,
                  humanAvatarUrls,
                );
                return (
                  <FilterSelectItem
                    key={facetValue(facet)}
                    value={facetValue(facet)}
                    icon={<SenderAvatar avatar={avatar} size={16} />}
                  >
                    {identity.label ?? t("conversationShell.system")}
                  </FilterSelectItem>
                );
              })
            )}
          </Select.Viewport>
        </Select.Content>
      </Select.Portal>
    </Select.Root>
  );
}

// ── Row ──────────────────────────────────────────────────────────────────

interface AttachmentRowProps {
  item: AttachmentListItem;
  presentation: AuthorPresentation | null;
  /** Byline avatars derived from each row's own source author DTO
   *  (#task:1792 B2) — not a directory join. */
  authorAgentAvatarUrls: Map<string, string | null>;
  authorHumanAvatarUrls: Map<string, string | null>;
  /** Route an md/html row to the in-app previewer tab (see the panel prop). */
  onOpenAttachment?: (attachment: AttachmentDto) => void;
  onJump: () => void;
}

/**
 * One attachment row. Three sibling <button>s inside a chip container:
 *   1. Main → Lightbox for images; the in-app previewer tab for md/html when a
 *      host is wired (`onOpenAttachment`, #task:2122); `open-with-OS` IPC for
 *      every other file type.
 *   2. Download → save-as via `attachment:save` IPC.
 *   3. Show-in-conversation → jump to the source message.
 *
 * Siblings, never nested; never a `role="button"` on a non-button. Root is
 * a `<div>` (not `<li>`): the enclosing GroupedVirtuoso items are `<div>`s
 * by default, so a lone `<li>` would be orphaned from any `<ul>`/`<ol>`.
 */
function AttachmentRow({
  item,
  presentation,
  authorAgentAvatarUrls,
  authorHumanAvatarUrls,
  onOpenAttachment,
  onJump,
}: AttachmentRowProps): JSX.Element {
  const { attachment, source } = item;
  const isImage = attachment.content_type.startsWith("image/");
  const appeared = dayjs(source.appeared_at);
  const [lightboxOpen, setLightboxOpen] = useState(false);
  const senderLabel = presentation?.label ?? t("conversationShell.system");

  // Image click flow — prefetch the blob URL BEFORE opening Lightbox so a
  // failed fetch surfaces as a toast (not a silently-closed modal). Only
  // opens the host on success; the host's own catch then bounds the narrow
  // post-open window where a just-cached URL could be re-evicted before
  // Lightbox's `<img>` decodes.
  async function openImagePreview(): Promise<void> {
    try {
      await loadAttachmentBlobUrl(attachment.id);
      setLightboxOpen(true);
    } catch (err) {
      toast.error(formatOpenException(err));
    }
  }

  return (
    <div>
      <CollectionItemCard className="flex w-full items-center gap-1.5 overflow-hidden px-4 py-3">
        {/* Sibling 1 — main preview / open. Hover = ink darkening on the
            filename + row lift (structural signal); no bg reversal. */}
        <button
          type="button"
          disabled={
            !isImage &&
            !(
              onOpenAttachment &&
              attachmentPreviewKind(attachment.content_type, attachment.filename)
            ) &&
            !getPlatform().files.openAttachmentExternal
          }
          onClick={() => {
            if (isImage) {
              void openImagePreview();
              return;
            }
            // md/html → the primary pane's previewer tab when a host is wired;
            // ChannelView applies the final previewable/gate decision and falls
            // back to open-with-OS itself, so a non-member pane still opens.
            if (
              onOpenAttachment &&
              attachmentPreviewKind(attachment.content_type, attachment.filename)
            ) {
              onOpenAttachment(attachment);
              return;
            }
            void openAttachmentExternally(attachment);
          }}
          className="group flex min-w-0 flex-1 items-center gap-2.5 text-left focus-outline"
          title={`${attachment.filename} (${formatBytes(attachment.size)})`}
        >
          <AttachmentThumb attachment={attachment} isImage={isImage} />
          {/* Filename over byline when the surface is narrow; wide, the
              filename and this byline cluster flow as one left-hugging content
              block with the elastic gap after it (rule in `chrome.css`, keyed on
              the surface's own container width — never on which host mounted
              it). */}
          <span data-attachment-row-body className="flex min-w-0 flex-1 flex-col leading-tight">
            <span
              data-attachment-row-name
              className="truncate text-sm text-text-primary group-hover:text-text-primary"
            >
              {attachment.filename}
            </span>
            <span
              data-attachment-row-meta
              className="flex min-w-0 items-center gap-2 overflow-hidden text-xs text-text-muted"
            >
              <span data-attachment-row-author className="flex min-w-4 items-center gap-1.5">
                <span className="flex shrink-0">
                  <AuthorAvatarView
                    avatar={presentation?.avatar ?? { variant: "neutral-user" }}
                    size={16}
                    label={senderLabel}
                    agentAvatarUrls={authorAgentAvatarUrls}
                    humanAvatarUrls={authorHumanAvatarUrls}
                  />
                </span>
                {/* Author name carries its deleted state through the ONE shared
                    primitive (#task:1792 B3); a system-authored source has no
                    author DTO and reads "System". */}
                {presentation ? (
                  <AuthorName presentation={presentation} className="truncate" />
                ) : (
                  <span className="truncate">{senderLabel}</span>
                )}
              </span>
              <span data-attachment-row-size className="shrink-0">
                {formatBytes(attachment.size)}
              </span>
              <time
                data-attachment-row-time
                dateTime={new Date(source.appeared_at).toISOString()}
                title={appeared.format("YYYY-MM-DD HH:mm:ss")}
                className="flex shrink-0 items-center gap-1"
              >
                <ClockIcon className="size-3" />
                <span>
                  <span data-attachment-row-year>{appeared.format("YYYY-")}</span>
                  {appeared.format("MM-DD HH:mm")}
                </span>
              </time>
            </span>
          </span>
        </button>

        <button
          type="button"
          data-attachment-row-action
          data-sticker
          data-sticker-size="md"
          data-sticker-tone="neutral"
          onClick={() => void saveAttachmentToDisk(attachment)}
          className="flex size-7 shrink-0 items-center justify-center text-text-muted hover:text-text-primary focus-outline"
          title={t("conversationShell.downloadFile", { file: attachment.filename })}
          aria-label={t("conversationShell.downloadFile", { file: attachment.filename })}
        >
          <DownloadIcon className="size-3.5" />
        </button>

        <button
          type="button"
          data-attachment-row-action
          data-sticker
          data-sticker-size="md"
          data-sticker-tone="neutral"
          onClick={onJump}
          className="flex size-7 shrink-0 items-center justify-center text-text-muted hover:text-text-primary focus-outline"
          title={t("conversationShell.showInConversation", { file: attachment.filename })}
          aria-label={t("conversationShell.showInConversation", { file: attachment.filename })}
        >
          <OpenExternalIcon className="size-3.5" />
        </button>
      </CollectionItemCard>

      {isImage && lightboxOpen ? (
        <ImageLightboxHost
          attachment={attachment}
          open={lightboxOpen}
          onOpenChange={setLightboxOpen}
        />
      ) : null}
    </div>
  );
}

// ── Thumbnail (viewport-lazy) ────────────────────────────────────────────

/** 32×32 thumbnail cell. For image attachments it loads the blob URL only
 *  once mounted (which, under `GroupedVirtuoso`, means "in viewport"), so
 *  a 5000-row channel doesn't fire 5000 concurrent blob fetches. The blob
 *  cache is shared with `MessageItem`, so scrolling to an already-viewed
 *  attachment is a synchronous cache hit. */
function AttachmentThumb({
  attachment,
  isImage,
}: {
  attachment: AttachmentDto;
  isImage: boolean;
}): JSX.Element {
  const [imageUrl, setImageUrl] = useState<string | null>(() =>
    isImage ? (getAttachmentBlobUrl(attachment.id) ?? null) : null,
  );
  // Generation token (#task:1769): a content update or gap-recovery bump swaps
  // the fresh bytes in place without unmounting the thumbnail.
  const token = useAttachmentBlobToken(attachment.id);
  const loadedTokenRef = useRef(token);
  // Stale-URL recovery is bounded: the shared blob-cache LRU may evict a
  // URL while an `<img>` still holds it, at which point the browser fires
  // `onError`. We refetch AT MOST ONCE per generation — if the second attempt
  // also fails, we give up and show the icon fallback, so a permanently
  // broken blob can't produce an infinite fetch→revoke→error loop.
  const retriedRef = useRef(false);
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    // A new generation (#task:1769: in-place content update or gap-recovery
    // bump) re-allows load/retry — an undecodable intermediate revision must
    // not permanently poison a later fixed one. Don't drop the visible old
    // image; the reload swaps it in place below.
    retriedRef.current = false;
    setFailed(false);
  }, [token]);

  useEffect(() => {
    if (!isImage || failed) return;
    // Already showing bytes for the current generation — nothing to do.
    if (imageUrl && loadedTokenRef.current === token) return;
    let cancelled = false;
    loadAttachmentBlobUrl(attachment.id)
      .then((url) => {
        if (!cancelled) {
          loadedTokenRef.current = token;
          setImageUrl(url);
          setFailed(false);
        }
      })
      .catch(() => {
        if (!cancelled) setFailed(true);
      });
    return () => {
      cancelled = true;
    };
  }, [attachment.id, isImage, imageUrl, failed, token]);

  function onImgError(): void {
    if (retriedRef.current) {
      // Second failure — the blob is gone. Fall back to the muted icon
      // and STOP; do not clear `imageUrl` again (that would kick the
      // effect above into another fetch cycle).
      setFailed(true);
      setImageUrl(null);
      return;
    }
    retriedRef.current = true;
    // Drop the stale URL so the effect above refetches through the
    // shared cache (which will re-populate the LRU under a fresh URL).
    setImageUrl(null);
  }

  if (isImage && imageUrl && !failed) {
    return (
      <span className="flex size-8 shrink-0 items-center justify-center overflow-hidden rounded-(--chip-radius) bg-bg-inset">
        <img
          src={imageUrl}
          alt={attachment.filename}
          onError={onImgError}
          className="block size-full object-cover"
        />
      </span>
    );
  }
  const Icon = isImage ? ImageIcon : DocumentIcon;
  return (
    <span className="flex size-8 shrink-0 items-center justify-center rounded-(--chip-radius) bg-bg-inset text-text-muted">
      <Icon className="size-4" />
    </span>
  );
}

/** Lightbox host — invoked only after the click-time prefetch has
 *  already succeeded, so its first pass typically hits the shared blob
 *  cache synchronously. Its own load only runs when the URL was evicted
 *  between the click and mount (the narrow post-open failure window). A
 *  failure here surfaces as a toast — using the shared exception
 *  formatter so a blob-fetch throw reads identically to
 *  `openAttachmentExternally`'s catch — and closes the modal.
 *  Toast dedup is by construction: the click-side toast and this one
 *  fire on strictly disjoint failure windows. */
function ImageLightboxHost({
  attachment,
  open,
  onOpenChange,
}: {
  attachment: AttachmentDto;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}): JSX.Element | null {
  const [url, setUrl] = useState<string | null>(() => getAttachmentBlobUrl(attachment.id) ?? null);
  // Refresh in place if the bytes are updated while the lightbox is open
  // (#task:1769) — the "staring at a preview, no surrounding re-render" case.
  const token = useAttachmentBlobToken(attachment.id);
  const loadedTokenRef = useRef(token);
  useEffect(() => {
    if (url && loadedTokenRef.current === token) return;
    let cancelled = false;
    loadAttachmentBlobUrl(attachment.id)
      .then((next) => {
        if (!cancelled) {
          loadedTokenRef.current = token;
          setUrl(next);
        }
      })
      .catch((err) => {
        if (cancelled) return;
        toast.error(formatOpenException(err));
        onOpenChange(false);
      });
    return () => {
      cancelled = true;
    };
  }, [attachment.id, url, onOpenChange, token]);
  if (!url) return null;
  return (
    <Lightbox
      open={open}
      onOpenChange={onOpenChange}
      src={url}
      loadExportBytes={async () => (await fetchAttachmentBytes(attachment.id)).bytes}
      attachmentId={attachment.id}
      alt={attachment.filename}
      onDownload={() => saveAttachmentToDisk(attachment)}
    />
  );
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${Math.round(n / 1024)} KB`;
  if (n < 1024 * 1024 * 1024) return `${Math.round(n / 1024 / 1024)} MB`;
  return `${Math.round(n / 1024 / 1024 / 1024)} GB`;
}
