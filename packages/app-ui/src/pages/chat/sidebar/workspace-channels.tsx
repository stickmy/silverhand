import { SidebarRowsSkeleton } from "@/components/layout/sidebar-skeleton";
import { SidebarReveal, SidebarRevealRow } from "@/components/layout/sidebar-reveal";
import { useMemo, useRef, type ReactNode } from "react";
import { type ChannelAttentionModeRowDto, type ConversationVisibility } from "@silverhand/shared";
import { useSortable } from "@dnd-kit/sortable";
import { ChannelRow } from "./channel-row";
import { AppContextMenu } from "@/components/menus/app-context-menu";
import { ChannelRowMenuItems } from "./channel-row-menu-items";
import { SidebarEmptyRow } from "@/components/layout/sidebar-shared";
import { SortModeMenu } from "@/components/layout/sort-mode-menu";
import {
  SortableGroup,
  animateLayoutChanges,
  sortableItemStyle,
} from "@/components/layout/list-drag-sort";
import { useListDragSort } from "@/features/workspaces/use-list-drag-sort";
import {
  useArchiveChannel,
  useChannelSortState,
  useChannelsForWorkspace,
  useSetSortMode,
  useStarChannel,
} from "@/features/workspaces/hooks";
import { useMessageUnreadCounts } from "@/features/channels/message-unread";
import { useWorkspaceAttentionModes } from "@/features/attention/hooks";
import { channelNodeBadge } from "@/features/attention/row-badges";
import { sortModeOf } from "@/lib/viewer-sort";
import { collapsedRevealRows, excludeStarredChannels } from "@/features/workspaces/home-grouping";
import { t } from "@/lib/i18n";

interface ChannelNode {
  id: string;
  title: string;
  roster_policy: "open" | "fixed";
  visibility: ConversationVisibility;
  starred_at: number | null;
  archived_at: number | null;
  created_at: number;
  is_member?: boolean;
}

export function ChannelSortMenu({ workspaceId }: { workspaceId: string }): JSX.Element {
  const { data: viewerSort } = useChannelSortState(workspaceId, true);
  const setSortMode = useSetSortMode();
  const listKey = `channels:${workspaceId}`;
  return (
    <SortModeMenu
      mode={sortModeOf(viewerSort)}
      onChange={(m) => setSortMode.mutate({ list_key: listKey, mode: m })}
    />
  );
}

interface WorkspaceChannelsProps {
  workspaceId: string;
  /** The conversation the tree is currently FILLing, or null. One id for the
   *  whole tree (the spec's single-highlight rule), so at most one row in one
   *  workspace can match. */
  selectedChannelId: string | null;
  onSelectChannel: (channelId: string, workspaceId: string) => void;
  /** COLLAPSED partial render: show only a subset — no drag, no sort, no empty-state
   *  action (see the file header). Always keeps the open channel's row; a starred
   *  selection is excluded and shows in STARRED, so it may render nothing. Both a
   *  collapsed SECTION and a collapsed BRANCH pass this. */
  collapsed?: boolean;
  /** Poke this branch's `loud` unread rows out on top of the kept selection
   *  (#task:1939). Read ONLY while `collapsed`. MUST be the branch's WAKE GATE (the
   *  lit workspace badge), never `!expanded`: a branch collapses-and-mounts to keep
   *  the selection with a DARK badge, and gating the poke on "collapsed" would reveal
   *  a stale `loud` sibling with no wake signal (`collapsedRevealRows` docblock). */
  pokeUnread?: boolean;
  /** A leading node (a workspace caption) shown ABOVE the revealed rows in the
   *  collapsed render — but ONLY when the reveal set is non-empty (#task:1939, @Cindy
   *  MUST FIX). It is co-fated with the rows on purpose: the collapsed GROUP mounts a
   *  branch on its WAKE GATE (`workspace.unread > 0`, an aggregate), which can be lit
   *  while this branch's row-level reveal is empty — its only loud row is STARRED
   *  (moved out), or the row queries are loading / failed. Rendering the caption
   *  beside the branch (before this component) would then leave a lone title with no
   *  channels under it — a visible signal with nothing to point at, breaking
   *  "starred-only only spins, shows no signal" and "loading/failure fabricates
   *  nothing". Passing it HERE binds it to the same `revealed.length > 0` gate the
   *  rows use, so no-rows ⟹ no-caption by construction. Only the collapsed GROUP
   *  passes it (to attribute a non-current branch's rows); the branch level does not. */
  revealCaption?: ReactNode;
}

export function WorkspaceChannels({
  workspaceId,
  selectedChannelId,
  onSelectChannel,
  collapsed = false,
  pokeUnread = false,
  revealCaption,
}: WorkspaceChannelsProps): JSX.Element {
  const mountedCollapsed = useRef(collapsed);
  const { data: items = [], isLoading } = useChannelsForWorkspace(workspaceId, true);
  const { data: viewerSort } = useChannelSortState(workspaceId, true);

  const { data: attentionModes } = useWorkspaceAttentionModes(workspaceId);
  const { data: unreadData } = useMessageUnreadCounts(workspaceId, true);
  const starChannel = useStarChannel(workspaceId);
  const archiveChannel = useArchiveChannel(workspaceId);

  const attentionByChannel = useMemo(
    () => new Map((attentionModes?.channels ?? []).map((c) => [c.conversation_id, c])),
    [attentionModes],
  );

  const workspaceMuted = attentionModes?.muted;
  const unreadByChannel = unreadData?.by_conversation ?? {};

  // `starred_at` is viewer-local sort state, never a channel field (#task:1209).
  // The old second column got it joined at the composition root; here the join
  // is local — this subtree already holds both halves of the same cache entry,
  // and a per-workspace join is what lets many workspaces render at once without
  // app.tsx keeping one map per workspace.
  const nodes = useMemo<ChannelNode[]>(() => {
    const starredAt = new Map<string, number>();
    for (const e of viewerSort?.entries ?? []) {
      if (e.starred_at !== null) starredAt.set(e.entity_id, e.starred_at);
    }
    return items.map((s) => ({
      id: s.id,
      title: s.title,
      roster_policy: s.roster_policy,
      visibility: s.visibility,
      is_member: s.is_member,
      starred_at: starredAt.get(s.id) ?? null,
      archived_at: s.archived_at,
      created_at: s.created_at,
    }));
  }, [items, viewerSort]);

  const homeTopLevel = excludeStarredChannels(nodes);
  const { orderedGroups, onReorder } = useListDragSort<"channels", ChannelNode>({
    listKey: `channels:${workspaceId}`,
    groups: { channels: homeTopLevel },
    viewerSort,
    getId: (c) => c.id,
    getCreatedAt: (c) => c.created_at,
  });
  const ordered = orderedGroups.channels;

  if (collapsed) {
    const revealed = collapsedRevealRows(ordered, {
      pokeUnread,
      isSelected: (c) => c.id === selectedChannelId,
      isLoud: (c) =>
        channelNodeBadge(c, attentionByChannel.get(c.id), unreadByChannel[c.id] ?? 0)?.tier ===
        "loud",
    });
    return (
      <>
        {revealed.length > 0 ? revealCaption : null}
        <SidebarReveal initial={mountedCollapsed.current}>
          {revealed.map((c) => (
            <SidebarRevealRow key={c.id}>
              <WorkspaceChannelRow
                channel={c}
                selected={c.id === selectedChannelId}
                onClick={() => onSelectChannel(c.id, workspaceId)}
                onStar={(channelId, starred) => starChannel.mutate({ channelId, starred })}
                onArchive={(channelId, archived) =>
                  archiveChannel.mutate({ channelId, archived, workspaceId })
                }
                archivePending={archiveChannel.isPending}
                unreadCount={unreadByChannel[c.id] ?? 0}
                attention={attentionByChannel.get(c.id)}
                workspaceMuted={workspaceMuted}
              />
            </SidebarRevealRow>
          ))}
        </SidebarReveal>
      </>
    );
  }

  if (isLoading && items.length === 0) {
    return (
      <li>
        <SidebarRowsSkeleton label={t("chat.loadingChannels")} nested />
      </li>
    );
  }

  if (ordered.length === 0) {
    return <SidebarEmptyRow>{t("chat.noChannels")}</SidebarEmptyRow>;
  }

  const rowProps = {
    selectedChannelId,
    onSelect: (channelId: string) => onSelectChannel(channelId, workspaceId),
    onStar: (channelId: string, starred: boolean) => starChannel.mutate({ channelId, starred }),
    onArchive: (channelId: string, archived: boolean) =>
      archiveChannel.mutate({ channelId, archived, workspaceId }),
    archivePending: archiveChannel.isPending,
    unreadByChannel,
    attentionByChannel,
    workspaceMuted,
  };

  return (
    <SortableGroup ids={ordered.map((c) => c.id)} onReorder={(next) => onReorder("channels", next)}>
      {ordered.map((channel) => (
        <SortableChannelItem key={channel.id} channel={channel} {...rowProps} />
      ))}
    </SortableGroup>
  );
}

interface RowChildProps {
  selectedChannelId: string | null;
  onSelect: (id: string) => void;
  onStar: (channelId: string, starred: boolean) => void;
  onArchive: (channelId: string, archived: boolean) => void;
  archivePending: boolean;
  unreadByChannel: Record<string, number>;
  /** This workspace's channel tiers, keyed by conversation id. Missing key ≡ the
   *  row has no tier of its own (a `dm_channel`, or the batch hasn't landed). */
  attentionByChannel: ReadonlyMap<string, ChannelAttentionModeRowDto>;
  /** The workspace suppressor, `undefined` until the tier batch resolves. Rows keep
   *  their own stored tier checked while it is on; only the resolved volume
   *  changes. */
  workspaceMuted: boolean | undefined;
}

function SortableChannelItem({
  channel,
  selectedChannelId,
  onSelect,
  onStar,
  onArchive,
  archivePending,
  unreadByChannel,
  attentionByChannel,
  workspaceMuted,
}: { channel: ChannelNode } & RowChildProps): JSX.Element {
  const { setNodeRef, transform, transition, isDragging, attributes, listeners } = useSortable({
    id: channel.id,
    animateLayoutChanges,
  });
  return (
    <li
      ref={setNodeRef}
      data-sidebar-sort-dragging={isDragging || undefined}
      style={sortableItemStyle({ transform, transition, isDragging })}
      className="touch-manipulation"
      {...attributes}
      {...listeners}
    >
      <WorkspaceChannelRow
        channel={channel}
        selected={channel.id === selectedChannelId}
        onClick={() => onSelect(channel.id)}
        onStar={onStar}
        onArchive={onArchive}
        archivePending={archivePending}
        unreadCount={unreadByChannel[channel.id] ?? 0}
        attention={attentionByChannel.get(channel.id)}
        workspaceMuted={workspaceMuted}
        draggable
      />
    </li>
  );
}

function WorkspaceChannelRow({
  channel,
  selected,
  onClick,
  onStar,
  onArchive,
  archivePending,
  unreadCount,
  attention,
  workspaceMuted,
  draggable,
}: {
  channel: ChannelNode;
  selected: boolean;
  onClick: () => void;
  onStar: (channelId: string, starred: boolean) => void;
  onArchive: (channelId: string, archived: boolean) => void;
  archivePending: boolean;
  unreadCount: number;
  attention?: ChannelAttentionModeRowDto;
  workspaceMuted: boolean | undefined;
  draggable?: boolean;
}): JSX.Element {
  const isArchived = channel.archived_at !== null;

  const badge = channelNodeBadge(channel, attention, unreadCount);

  const isSilenced = attention?.mode === "mute";

  const row = (
    <ChannelRow
      selected={selected}
      column="nested"
      draggable={draggable}
      onSelect={onClick}
      channelTitle={channel.title}
      visibility={channel.roster_policy === "open" ? channel.visibility : undefined}
      muted={isSilenced}
      badge={badge}
    />
  );

  return (
    <AppContextMenu trigger={row}>
      <ChannelRowMenuItems
        channelId={channel.id}
        title={channel.title}
        rosterPolicy={channel.roster_policy}
        isMember={channel.is_member === true}
        isArchived={isArchived}
        starred={false}
        attention={attention}
        workspaceMuted={workspaceMuted}
        onStar={() => onStar(channel.id, true)}
        onArchive={() => onArchive(channel.id, !isArchived)}
        archivePending={archivePending}
      />
    </AppContextMenu>
  );
}

export type { ChannelNode };
