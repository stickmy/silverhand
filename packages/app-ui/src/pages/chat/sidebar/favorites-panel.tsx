import { SidebarRowsSkeleton } from "@/components/layout/sidebar-skeleton";
import { SidebarReveal, SidebarRevealRow } from "@/components/layout/sidebar-reveal";
import { useMemo } from "react";
import { StarIcon } from "@/icons";
import type {
  ChannelAttentionModeRowDto,
  DirectPairFavoriteRowDto,
  FavoriteRowDto,
} from "@silverhand/shared";
import { useSortable } from "@dnd-kit/sortable";
import { ChannelRowMenuItems } from "./channel-row-menu-items";
import { useFavorites, useUnstarFavorite } from "@/features/workspaces/favorites";
import { useAttentionModesAcrossWorkspaces } from "@/features/attention/hooks";
import { collapsedRevealRows, type FavoritesWatchScope } from "@/features/workspaces/home-grouping";
import { useArchiveChannel, useChannelDetail, useSetSortMode } from "@/features/workspaces/hooks";
import { favoriteRowBadge } from "@/features/attention/row-badges";
import { conversationSelectionTier } from "@/pages/chat/model/rail-selection";
import { sortModeOf } from "@/lib/viewer-sort";
import { SortModeMenu } from "@/components/layout/sort-mode-menu";
import {
  SortableGroup,
  animateLayoutChanges,
  useSortableRowChrome,
} from "@/components/layout/list-drag-sort";
import { useListDragSort } from "@/features/workspaces/use-list-drag-sort";
import { SidebarSection, SidebarEmptyRow } from "../../../components/layout/sidebar-shared";
import { useAgentAvatarObjectUrls } from "@/features/agents/hooks/avatar-hooks";
import { useUserAvatarObjectUrls } from "@/features/users/avatar-hooks";
import { collectCounterpartAvatarSources } from "@/features/direct-pairs/counterpart-presentation";
import { directConversation } from "./direct-messages-panel";
import { SidebarConversationRow, type SidebarConversation } from "./sidebar-conversation";
import { ChannelIcon } from "@/components/channel-icon";
import { t } from "@/lib/i18n";

/** One STARRED row: a starred channel or a starred direct pair, one sort group. */
type StarredRow =
  | { kind: "channel"; key: string; createdAt: number; item: FavoriteRowDto }
  | { kind: "direct"; key: string; createdAt: number; pair: DirectPairFavoriteRowDto };

interface AvatarUrls {
  agents: ReadonlyMap<string, string | null>;
  humans: ReadonlyMap<string, string | null>;
}

const FAVORITES_LIST_KEY = "favorites";
const FAVORITES_GROUP = "favorites";

export function FavoritesGroup(props: {
  /** The open conversation — the ONLY thing that highlights a STARRED row. */
  selectedChannelId: string | null;
  /** Every workspace the viewer can currently see. This list's freshness
   *  subscription is scoped to them (see `useFavorites`) — the set must come from
   *  OUTSIDE the projection, because a parent with no visible favorite is exactly
   *  the one holding a hidden star waiting to be restored. `FavoritesWatchScope`
   *  is what enforces that: its only producer is
   *  `partitionRailWorkspaces().allVisibleIds`, so a projection-derived
   *  `string[]` cannot be passed here. */
  watchWorkspaceIds: FavoritesWatchScope;
  /** Navigate to a starred conversation inside its parent workspace. */
  onSelectConversation: (workspaceId: string, conversationId: string) => void;
  /** Open a starred direct pair. */
  onSelectDirectPair: (conversationId: string) => void;
}): JSX.Element | null {
  const { data, isLoading } = useFavorites(props.watchWorkspaceIds);
  const items = useMemo(() => data?.items ?? [], [data]);
  const pairs = useMemo(() => data?.direct_pairs ?? [], [data]);
  const rows = useMemo<StarredRow[]>(
    () => [
      ...items.map((item): StarredRow => ({
        kind: "channel",
        key: item.favorite_key,
        createdAt: item.created_at,
        item,
      })),
      ...pairs.map((pair): StarredRow => ({
        kind: "direct",
        key: pair.favorite_key,
        createdAt: pair.created_at,
        pair,
      })),
    ],
    [items, pairs],
  );
  const avatarSources = useMemo(() => collectCounterpartAvatarSources(pairs), [pairs]);
  const avatars: AvatarUrls = {
    agents: useAgentAvatarObjectUrls(avatarSources.agents),
    humans: useUserAvatarObjectUrls(avatarSources.users),
  };
  const viewerSort = data?.viewer_sort;
  const unstar = useUnstarFavorite();
  const setSortMode = useSetSortMode();

  // Tier authority for the rows that HAVE a tier. Only open-roster parents
  // are fanned out: the modes population joins `conversation_users` AND
  // `c.roster_policy='open'`, so a fixed-roster row's parent (an agent home) would answer with
  // an empty batch every time — zero information, and one request per DM. A DM
  // channel's verdict is `'all'` by backend rule, resolved on the row below.
  const channelParentIds = useMemo(
    () =>
      Array.from(
        new Set(items.filter((i) => i.roster_policy === "open").map((i) => i.workspace_id)),
      ).sort(),
    [items],
  );
  const { byChannel, mutedByWorkspace } = useAttentionModesAcrossWorkspaces(channelParentIds);

  // Compose the live order from the favorites `viewer_sort` (recent → home
  // last_accessed_at, manual → rank), reusing the shared drag-sort machine. A single
  // flat group; list-wide mode scope (no `modeScopeOf`), same as the channel sidebar.
  // `getId` MUST be `favorite_key` — it is the entity_id under which
  // `viewer_sort.entries` (and the backend rank) live, and the reorder `ordered_ids`
  // element.
  const { orderedGroups, onReorder } = useListDragSort<typeof FAVORITES_GROUP, StarredRow>({
    listKey: FAVORITES_LIST_KEY,
    groups: { [FAVORITES_GROUP]: rows },
    viewerSort,
    getId: (r) => r.key,
    getCreatedAt: (r) => r.createdAt,
  });
  const ordered = orderedGroups[FAVORITES_GROUP];
  const mode = sortModeOf(viewerSort);

  const sidebarOf = (row: StarredRow): SidebarConversation => {
    if (row.kind === "direct") {
      return directConversation(
        row.pair.conversation_id,
        { counterpart: row.pair.counterpart, unread: row.pair.unread_count },
        { agentAvatarUrls: avatars.agents, humanAvatarUrls: avatars.humans },
        true,
        () => props.onSelectDirectPair(row.pair.conversation_id),
      );
    }
    const { item } = row;
    const attention = byChannel.get(item.entity_id);
    return {
      id: item.entity_id,
      workspaceId: item.workspace_id,
      face: {
        glyph: (
          <ChannelIcon
            variant="sticker"
            visibility={item.roster_policy === "open" ? item.visibility : undefined}
          />
        ),
        title: item.title,
        badge: favoriteRowBadge(item, attention),
      },
      muted: attention?.mode === "mute",
      tooltip: `${item.title} — ${item.parent_title}`,
      open: () => props.onSelectConversation(item.workspace_id, item.entity_id),
      menu: (
        <FavoriteMenuItems
          item={item}
          attention={attention}
          workspaceMuted={mutedByWorkspace.get(item.workspace_id)}
          onUnstar={() => unstar.mutate(item)}
        />
      ),
    };
  };
  const lines = ordered.map((row) => ({ row, conversation: sidebarOf(row) }));

  // Revealed rows keep the full-list order, but cannot reorder a partial set.
  const revealed = collapsedRevealRows(lines, {
    pokeUnread: true,
    isSelected: ({ row }) => conversationOf(row) === props.selectedChannelId,
    isLoud: ({ conversation }) => conversation.face.badge?.tier === "loud",
  });

  const renderLine = (
    { row, conversation }: (typeof lines)[number],
    sortable: boolean,
  ): JSX.Element => (
    <SidebarConversationRow
      conversation={conversation}
      selected={
        conversationSelectionTier(conversationOf(row), props.selectedChannelId) === "primary"
      }
      draggable={sortable}
    />
  );

  return (
    <SidebarSection
      label={t("chat.starred")}
      labelClassName="text-xs"
      icon={<StarIcon className="size-3.5" />}
      count={rows.length}
      collapsible
      defaultExpanded
      persistId="starred"
      collapsedContent={
        <SidebarReveal>
          {revealed.map((line) => (
            <SidebarRevealRow key={line.row.key}>{renderLine(line, false)}</SidebarRevealRow>
          ))}
        </SidebarReveal>
      }
      action={
        <SortModeMenu
          mode={mode}
          onChange={(m) => setSortMode.mutate({ list_key: FAVORITES_LIST_KEY, mode: m })}
        />
      }
    >
      {ordered.length === 0 ? (
        isLoading ? (
          <li>
            <SidebarRowsSkeleton label={t("chat.loadingStarredConversations")} nested />
          </li>
        ) : (
          <SidebarEmptyRow>{t("chat.noStarredConversations")}</SidebarEmptyRow>
        )
      ) : null}
      <SortableGroup
        ids={lines.map(({ row }) => row.key)}
        onReorder={(next) => onReorder(FAVORITES_GROUP, next)}
      >
        {lines.map((line) => (
          <SortableStarredRow key={line.row.key} id={line.row.key}>
            {renderLine(line, true)}
          </SortableStarredRow>
        ))}
      </SortableGroup>
    </SidebarSection>
  );
}

const conversationOf = (row: StarredRow): string =>
  row.kind === "channel" ? row.item.entity_id : row.pair.conversation_id;

interface FavoriteMenuProps {
  item: FavoriteRowDto;
  /** This conversation's tier row from its parent's batch. Absent ≡ no tier of its
   *  own (a `dm_channel`, a browse-only public channel, or the batch hasn't landed) —
   *  never defaulted here. */
  attention: ChannelAttentionModeRowDto | undefined;
  /** The parent workspace's suppressor, `undefined` until its batch resolves. Only
   *  reaches the Notifications sub-menu, which needs it to explain a checked row. */
  workspaceMuted: boolean | undefined;
  onUnstar: () => void;
}

/** The drag wrapper: an `<li>` bound to dnd-kit by `favorite_key` (the same id in
 *  `SortableGroup ids` + the backend rank), mirroring the channel sidebar's
 *  `SortableChannelItem`. Drag listeners sit on the `<li>`; the row's own click
 *  (navigate) and context menu (unstar) still fire — dnd-kit's activation constraint
 *  separates a drag from a click. */
function SortableStarredRow(props: { id: string; children: JSX.Element }): JSX.Element {
  const { setNodeRef, transform, transition, isDragging, attributes, listeners } = useSortable({
    id: props.id,
    animateLayoutChanges,
  });
  const chrome = useSortableRowChrome({ transform, transition, isDragging });
  return (
    <li ref={setNodeRef} {...chrome} className="touch-manipulation" {...attributes} {...listeners}>
      {props.children}
    </li>
  );
}

/** Mounted by the menu portal: resolve membership only when actions are requested. */
function FavoriteMenuItems({
  item,
  attention,
  workspaceMuted,
  onUnstar,
}: FavoriteMenuProps): JSX.Element {
  const { data } = useChannelDetail(item.entity_id);
  const archive = useArchiveChannel(item.workspace_id);
  const isArchived = data?.channel.archived_at != null;
  return (
    <ChannelRowMenuItems
      channelId={item.entity_id}
      title={item.title}
      rosterPolicy={item.roster_policy}
      isMember={data?.is_member === true}
      isArchived={isArchived}
      starred
      attention={attention}
      workspaceMuted={workspaceMuted}
      onStar={onUnstar}
      onArchive={() =>
        archive.mutate({
          channelId: item.entity_id,
          archived: !isArchived,
          workspaceId: item.workspace_id,
        })
      }
      archivePending={archive.isPending}
    />
  );
}
