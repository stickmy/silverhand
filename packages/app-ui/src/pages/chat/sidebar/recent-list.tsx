import {
  forwardRef,
  useMemo,
  useState,
  type ButtonHTMLAttributes,
  type ReactElement,
  type ReactNode,
} from "react";
import { useQueries } from "@tanstack/react-query";
import type {
  AgentDto,
  ChannelAttentionModeRowDto,
  DirectPairCounterpart,
} from "@silverhand/shared";
import { cn } from "@/lib/cn";
import { AVATAR_PX } from "@/components/avatar-scale";
import { LockIcon, PlusIcon, SettingsIcon, StarIcon, ThreadIcon } from "@/icons";
import { Button } from "@/components/ui/button";
import { WorkspaceIcon } from "@/components/workspace-icon";
import { isAgentSpace } from "@/features/workspaces/workspace-ownership";
import { MutedMarker, UnreadBadge } from "@/components/layout/sidebar-shared";
import { SidebarRowsSkeleton } from "@/components/layout/sidebar-skeleton";
import type { RowBadge } from "@/features/attention/attention-mode";
import { useAttentionModesAcrossWorkspaces } from "@/features/attention/hooks";
import { messageUnreadByConversationQuery } from "@/features/channels/message-unread";
import {
  useConversationLatest,
  type ConversationLatest,
} from "@/features/conversations/latest/use-conversation-latest";
import { useDirectPairsList } from "@/features/direct-pairs/hooks";
import { directLinePairVisibleInSidebar } from "@/features/direct-pairs/direct-line-rows";
import {
  collectCounterpartAvatarSources,
  presentCounterpart,
} from "@/features/direct-pairs/counterpart-presentation";
import { useAgentAvatarObjectUrls } from "@/features/agents/hooks/avatar-hooks";
import { useUserAvatarObjectUrls } from "@/features/users/avatar-hooks";
import { AuthorAvatarView } from "@/features/messages/components/author-avatar-view";
import { ChannelAvatar } from "@/features/conversations/components/channel-avatar";
import { presentAuthor } from "@/features/messages/model/author-presentation";
import {
  useArchiveChannel,
  useChannelsAcrossWorkspaces,
  useStarChannel,
} from "@/features/workspaces/hooks";
import { AppContextMenu } from "@/components/menus/app-context-menu";
import { starredConversationIds, useFavoritesList } from "@/features/workspaces/favorites";
import { DirectPairStarMenuItem } from "./direct-messages-panel";
import { useViewerUser } from "@/features/session/hooks";
import { channelNodeBadge, directRowBadge } from "../model/sidebar-row-badges";
import {
  orderRecentRows,
  recentPreviewText,
  recentTimeLabel,
} from "@/features/conversations/latest/recent-rows";
import { ChannelRowMenuItems } from "./channel-row-menu-items";
import { ProjectFilterStrip } from "./project-filter-strip";
import type { WorkspaceItem } from "./workspaces-panel";
import { useCoarsePointer } from "@/navigation/layout";
import { t } from "@/lib/i18n";

type RecentListRow =
  | {
      kind: "channel";
      id: string;
      createdAt: number;
      latest: ConversationLatest | null;
      title: string;
      workspaceId: string;
      workspaceName: string;
      visibility: "public" | "private" | undefined;
      rosterPolicy: "open" | "fixed";
      attention: ChannelAttentionModeRowDto | undefined;
      workspaceMuted: boolean | undefined;
      badge: RowBadge | null;
      muted: boolean;
    }
  | {
      kind: "direct";
      id: string;
      createdAt: number;
      latest: ConversationLatest | null;
      counterpart: DirectPairCounterpart;
      badge: RowBadge | null;
    };

/**
 * The Recent sidebar: every conversation the viewer is in, newest message first,
 * pinned (starred) rows on top. Rows come from the same lists as the tree, so
 * leaving a conversation removes its row the way it does there; the latest
 * summary only supplies the preview line and the order.
 */
export function RecentList(props: {
  workspaces: readonly WorkspaceItem[];
  selectedConversationId: string | null;
  onSelectChannel: (channelId: string, workspaceId: string) => void;
  onSelectDirectPair: (conversationId: string) => void;
  /** Opens a thread reply where it was written. */
  onOpenThread: (target: {
    workspaceId: string | null;
    conversationId: string;
    threadRootId: string;
    messageId: string;
  }) => void;
  /** Icon actions held at the end of the filter row; null where a native bar
   *  carries them. */
  actions: ReactNode;
  onCreateChannel: (workspaceId: string) => void;
  onOpenWorkspaceSettings: (workspaceId: string) => void;
  agentsById?: ReadonlyMap<string, AgentDto>;
  ownerAgentAvatarUrls: ReadonlyMap<string, string | null>;
}): JSX.Element {
  const [workspaceFilter, setWorkspaceFilter] = useState<string | null>(null);

  const liveWorkspaces = useMemo(
    () => props.workspaces.filter((w) => !w.archived),
    [props.workspaces],
  );
  const focusedWorkspace =
    liveWorkspaces.find((w) => w.id === workspaceFilter) ??
    (liveWorkspaces.length === 1 ? liveWorkspaces[0]! : null);
  const workspaceIds = useMemo(() => liveWorkspaces.map((w) => w.id), [liveWorkspaces]);
  const workspaceNames = useMemo(
    () => new Map(liveWorkspaces.map((w) => [w.id, w.name])),
    [liveWorkspaces],
  );

  const channelLists = useChannelsAcrossWorkspaces(workspaceIds);
  const unreadLists = useQueries({
    queries: workspaceIds.map((id) => messageUnreadByConversationQuery(id)),
  });
  const { byChannel: attentionByChannel, mutedByWorkspace } =
    useAttentionModesAcrossWorkspaces(workspaceIds);
  const pairsQuery = useDirectPairsList();
  const favorites = useFavoritesList();
  const latest = useConversationLatest();
  const viewer = useViewerUser();

  const pinned = useMemo(() => starredConversationIds(favorites.data), [favorites.data]);

  const rows: RecentListRow[] = [];
  workspaceIds.forEach((workspaceId, index) => {
    const unread = unreadLists[index]?.data?.by_conversation ?? {};
    for (const channel of channelLists[index]?.data ?? []) {
      if (channel.is_member !== true || channel.archived_at !== null) {
        continue;
      }

      const attention: ChannelAttentionModeRowDto | undefined = attentionByChannel.get(channel.id);
      rows.push({
        kind: "channel",
        id: channel.id,
        createdAt: channel.created_at,
        latest: latest?.get(channel.id) ?? null,
        title: channel.title,
        workspaceId,
        workspaceName: workspaceNames.get(workspaceId) ?? "",
        visibility: channel.roster_policy === "open" ? channel.visibility : undefined,
        rosterPolicy: channel.roster_policy,
        attention,
        workspaceMuted: mutedByWorkspace.get(workspaceId),
        badge: channelNodeBadge(channel, attention, unread[channel.id] ?? 0),
        muted: attention?.mode === "mute" || mutedByWorkspace.get(workspaceId) === true,
      });
    }
  });
  for (const pair of pairsQuery.data?.direct_pairs ?? []) {
    if (!directLinePairVisibleInSidebar(pair)) {
      continue;
    }

    rows.push({
      kind: "direct",
      id: pair.conversation_id,
      createdAt: pair.created_at,
      latest: latest?.get(pair.conversation_id) ?? null,
      counterpart: pair.counterpart,
      badge: directRowBadge(pair.unread_count),
    });
  }

  const visible = orderRecentRows(
    workspaceFilter
      ? rows.filter((row) => row.kind === "channel" && row.workspaceId === workspaceFilter)
      : rows,
    pinned,
  );

  const directRows = visible.filter((row) => row.kind === "direct");
  const { agents, users } = collectCounterpartAvatarSources(directRows);
  const agentAvatarUrls = useAgentAvatarObjectUrls(agents);
  const humanAvatarUrls = useUserAvatarObjectUrls(users);

  const viewerId = viewer.data && "id" in viewer.data ? viewer.data.id : null;
  const now = Date.now();
  const loading =
    rows.length === 0 && (pairsQuery.isLoading || channelLists.some((list) => list.isLoading));

  return (
    <>
      {/* Pinned while the list scrolls: the filter and the entries beside it are
          how the list is steered, so they stay in reach. */}
      {liveWorkspaces.length > 1 || props.actions || focusedWorkspace ? (
        <div
          data-recent-filter
          className="sticky top-0 z-10 shrink-0 bg-(--pane-bg,var(--bg-base)) pb-2"
        >
          <div className="flex items-center gap-1">
            {liveWorkspaces.length > 1 ? (
              <ProjectFilterStrip
                workspaces={liveWorkspaces}
                value={workspaceFilter}
                onChange={setWorkspaceFilter}
                agentsById={props.agentsById}
                ownerAgentAvatarUrls={props.ownerAgentAvatarUrls}
              />
            ) : (
              <div className="flex-1" />
            )}
            {props.actions}
          </div>
          {/* The list has no project header, so the project in focus carries its
              own entries here, where the filter put it. */}
          {focusedWorkspace ? (
            <ProjectBar
              workspace={focusedWorkspace}
              agentsById={props.agentsById}
              ownerAgentAvatarUrls={props.ownerAgentAvatarUrls}
              onCreateChannel={() => props.onCreateChannel(focusedWorkspace.id)}
              onOpenSettings={
                isAgentSpace(focusedWorkspace)
                  ? undefined
                  : () => props.onOpenWorkspaceSettings(focusedWorkspace.id)
              }
            />
          ) : null}
        </div>
      ) : null}

      {loading ? <SidebarRowsSkeleton label={t("chat.loadingConversations")} /> : null}

      <ul className="flex flex-col">
        {visible.map((row, index) => (
          <li key={row.id}>
            <RecentRowMenu row={row} pinned={pinned.has(row.id)}>
              <RecentRow
                row={row}
                selected={row.id === props.selectedConversationId}
                pinned={pinned.has(row.id)}
                divider={
                  index < visible.length - 1 &&
                  row.id !== props.selectedConversationId &&
                  visible[index + 1]!.id !== props.selectedConversationId
                }
                viewerId={viewerId}
                now={now}
                agentAvatarUrls={agentAvatarUrls}
                humanAvatarUrls={humanAvatarUrls}
                onSelect={() => {
                  const workspaceId = row.kind === "channel" ? row.workspaceId : null;
                  if (row.latest?.thread_root_id) {
                    props.onOpenThread({
                      workspaceId,
                      conversationId: row.id,
                      threadRootId: row.latest.thread_root_id,
                      messageId: row.latest.message_id,
                    });
                    return;
                  }
                  if (row.kind === "channel") {
                    props.onSelectChannel(row.id, row.workspaceId);
                  } else {
                    props.onSelectDirectPair(row.id);
                  }
                }}
              />
            </RecentRowMenu>
          </li>
        ))}
      </ul>
    </>
  );
}

/** The project in focus, named the way the tree's project row names it, with the
 *  entries that row carries. */
function ProjectBar(props: {
  workspace: WorkspaceItem;
  agentsById?: ReadonlyMap<string, AgentDto>;
  ownerAgentAvatarUrls: ReadonlyMap<string, string | null>;
  onCreateChannel: () => void;
  onOpenSettings?: () => void;
}): JSX.Element {
  return (
    <div className="mt-2 flex h-7 items-center gap-2 pl-2">
      <WorkspaceIcon
        ownerAgentId={props.workspace.owner_agent_id}
        agentsById={props.agentsById}
        ownerAgentAvatarUrls={props.ownerAgentAvatarUrls}
      />
      <span className="min-w-0 flex-1 truncate text-sm font-semibold text-text-primary">
        {props.workspace.name}
      </span>
      <div className="flex shrink-0 items-center">
        <ProjectBarButton label={t("chat.newChannel")} onClick={props.onCreateChannel}>
          <PlusIcon className="size-4" />
        </ProjectBarButton>
        {props.onOpenSettings ? (
          <ProjectBarButton label={t("chat.projectSettings")} onClick={props.onOpenSettings}>
            <SettingsIcon className="size-4" />
          </ProjectBarButton>
        ) : null}
      </div>
    </div>
  );
}

function ProjectBarButton(props: {
  label: string;
  onClick: () => void;
  children: ReactNode;
}): JSX.Element {
  return (
    <Button
      type="button"
      variant="ghost"
      size="icon"
      aria-label={props.label}
      title={props.label}
      onClick={props.onClick}
      className="size-7 text-text-muted hover:text-text-primary"
    >
      {props.children}
    </Button>
  );
}

interface RecentRowProps extends Omit<ButtonHTMLAttributes<HTMLButtonElement>, "onSelect"> {
  row: RecentListRow;
  selected: boolean;
  /** Starred rows lead the list out of time order; the star says why. */
  pinned: boolean;
  /** A hairline under the text column, as in a native chat list; dropped next to
   *  the selected row, whose fill already separates it. */
  divider: boolean;
  viewerId: string | null;
  now: number;
  agentAvatarUrls: ReadonlyMap<string, string | null>;
  humanAvatarUrls: ReadonlyMap<string, string | null>;
  onSelect: () => void;
}

// Forwards the ref and the menu trigger's handlers to the button, which the
// context menu needs as its `asChild` trigger.
const RecentRow = forwardRef<HTMLButtonElement, RecentRowProps>(function RecentRow(
  {
    row,
    selected,
    pinned,
    divider,
    viewerId,
    now,
    agentAvatarUrls,
    humanAvatarUrls,
    onSelect,
    ...rest
  },
  ref,
) {
  // A touch list takes iOS list proportions, wide or not: a larger picture and
  // type, no hairlines, and unread told by weight with a number only when it
  // is loud.
  const compact = useCoarsePointer();
  const unread = row.badge !== null;

  const counterpart = row.kind === "direct" ? presentCounterpart(row.counterpart) : null;
  const title = row.kind === "channel" ? row.title : counterpart!.label;
  const latest = row.latest;
  const preview = latest ? recentPreviewText(latest) : "";
  const sender = latest ? presentAuthor(latest.sender) : null;
  // A DM's counterpart is implied by the row; only the viewer's own lines and
  // channel messages name who wrote them.
  const senderLabel =
    !sender || !latest
      ? null
      : sender.navigableId !== null && sender.navigableId === viewerId
        ? t("chat.you")
        : row.kind === "channel"
          ? sender.label
          : null;

  return (
    <button
      {...rest}
      ref={ref}
      type="button"
      onClick={onSelect}
      data-row-selected={selected || undefined}
      className={cn(
        "sidebar-row-border flex w-full items-center border px-2 text-left",
        compact ? "h-[72px] gap-3" : "h-14 gap-2.5",
        selected
          ? "navigation-row border-accent bg-bg-nav-selected text-text-nav-selected"
          : "navigation-row border-transparent text-text-secondary hover:border-accent hover:bg-bg-nav-hover hover:text-text-primary",
      )}
    >
      <span
        data-recent-avatar
        className={cn("flex shrink-0 items-center justify-center", compact ? "size-12" : "size-9")}
      >
        {counterpart ? (
          <AuthorAvatarView
            avatar={counterpart.avatar}
            size={compact ? AVATAR_PX["2xl"] : AVATAR_PX.lg}
            label={counterpart.label}
            agentAvatarUrls={agentAvatarUrls}
            humanAvatarUrls={humanAvatarUrls}
          />
        ) : (
          <ChannelAvatar
            id={row.id}
            title={title}
            size={compact ? AVATAR_PX["2xl"] : AVATAR_PX.lg}
          />
        )}
      </span>

      <span
        className={cn(
          "flex min-w-0 flex-1 flex-col justify-center gap-0.5 self-stretch border-b",
          divider && !compact ? "border-border-subtle" : "border-transparent",
        )}
      >
        <span className="flex min-w-0 items-baseline gap-1.5">
          <span
            className={cn(
              "min-w-0 truncate text-text-primary",
              compact ? "text-base leading-5" : "text-[13px] leading-4",
              selected || unread ? "font-semibold" : compact ? "font-normal" : "font-medium",
              selected && "text-text-nav-selected",
            )}
          >
            {title}
          </span>
          {!compact && row.kind === "channel" && row.visibility === "private" ? (
            <LockIcon
              role="img"
              aria-label={t("chat.private")}
              className="-mx-0.5 size-3 shrink-0 self-center text-text-muted"
            />
          ) : null}
          {row.kind === "channel" ? (
            <span className="min-w-0 shrink-[999] truncate text-(length:--text-meta) text-text-muted">
              {row.workspaceName}
            </span>
          ) : null}
          {pinned ? (
            <StarIcon
              role="img"
              aria-label={t("chat.starred2")}
              className="ml-auto size-3 shrink-0 self-center fill-current text-text-muted opacity-60"
            />
          ) : null}
          <span
            className={cn(
              "shrink-0 text-(length:--text-meta) tabular-nums text-text-muted",
              !pinned && "ml-auto",
            )}
          >
            {latest ? recentTimeLabel(latest.created_at, now) : ""}
          </span>
        </span>

        <span className="flex min-w-0 items-center gap-1.5">
          <span
            className={cn(
              "min-w-0 flex-1 truncate",
              compact ? "text-sm leading-5" : "text-xs leading-4",
              compact && unread ? "text-text-primary" : "text-text-muted",
            )}
          >
            {latest?.thread_root_id ? (
              <ThreadIcon
                role="img"
                aria-label={t("chat.inAThread")}
                className="mr-1 inline size-3 align-[-2px]"
              />
            ) : null}
            {senderLabel ? `${senderLabel}: ` : null}
            {preview}
          </span>
          {row.kind === "channel" && row.muted ? <MutedMarker /> : null}
          {row.badge && compact && row.badge.tier === "quiet" ? (
            <span
              role="img"
              aria-label={t("ui.unread")}
              className="size-2 shrink-0 rounded-full bg-(--unread)"
            />
          ) : row.badge ? (
            <UnreadBadge count={row.badge.count} tier={row.badge.tier} inverted={selected} />
          ) : null}
        </span>
      </span>
    </button>
  );
});

/** The tree's row menu on the same conversation here: the channel menu, or a
 *  direct pair's Star item. */
function RecentRowMenu(props: {
  row: RecentListRow;
  pinned: boolean;
  children: ReactElement;
}): JSX.Element {
  if (props.row.kind !== "channel") {
    return (
      <AppContextMenu trigger={props.children}>
        <DirectPairStarMenuItem conversationId={props.row.id} starred={props.pinned} />
      </AppContextMenu>
    );
  }

  return (
    <RecentChannelMenu row={props.row} pinned={props.pinned}>
      {props.children}
    </RecentChannelMenu>
  );
}

function RecentChannelMenu(props: {
  row: Extract<RecentListRow, { kind: "channel" }>;
  pinned: boolean;
  children: ReactElement;
}): JSX.Element {
  const { row, pinned } = props;
  const star = useStarChannel(row.workspaceId);
  const archive = useArchiveChannel(row.workspaceId);

  return (
    <AppContextMenu trigger={props.children}>
      <ChannelRowMenuItems
        channelId={row.id}
        title={row.title}
        rosterPolicy={row.rosterPolicy}
        isMember
        isArchived={false}
        starred={pinned}
        attention={row.attention}
        workspaceMuted={row.workspaceMuted}
        onStar={() => star.mutate({ channelId: row.id, starred: !pinned })}
        onArchive={() =>
          archive.mutate({ channelId: row.id, archived: true, workspaceId: row.workspaceId })
        }
        archivePending={archive.isPending}
      />
    </AppContextMenu>
  );
}
