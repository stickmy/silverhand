import { SidebarRowsSkeleton } from "@/components/layout/sidebar-skeleton";
import { SidebarReveal, SidebarRevealRow } from "@/components/layout/sidebar-reveal";
import { useMemo, useRef, type ReactNode } from "react";
import { type ConversationVisibility } from "@silverhand/shared";
import { useSortable } from "@dnd-kit/sortable";
import { ChannelIcon } from "@/components/channel-icon";
import { SidebarConversationRow, type SidebarConversation } from "./sidebar-conversation";
import { ChannelRowMenuItems } from "./channel-row-menu-items";
import { SidebarEmptyRow } from "@/components/layout/sidebar-shared";
import { SortModeMenu } from "@/components/layout/sort-mode-menu";
import {
  SortableGroup,
  animateLayoutChanges,
  useSortableRowChrome,
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

  const conversationOf = (c: ChannelNode): SidebarConversation => {
    const attention = attentionByChannel.get(c.id);
    const isArchived = c.archived_at !== null;
    return {
      id: c.id,
      workspaceId,
      face: {
        glyph: (
          <ChannelIcon
            variant="sticker"
            visibility={c.roster_policy === "open" ? c.visibility : undefined}
          />
        ),
        title: c.title,
        badge: channelNodeBadge(c, attention, unreadByChannel[c.id] ?? 0),
      },
      muted: attention?.mode === "mute",
      open: () => onSelectChannel(c.id, workspaceId),
      menu: (
        <ChannelRowMenuItems
          channelId={c.id}
          title={c.title}
          rosterPolicy={c.roster_policy}
          isMember={c.is_member === true}
          isArchived={isArchived}
          starred={false}
          attention={attention}
          workspaceMuted={workspaceMuted}
          onStar={() => starChannel.mutate({ channelId: c.id, starred: true })}
          onArchive={() =>
            archiveChannel.mutate({ channelId: c.id, archived: !isArchived, workspaceId })
          }
          archivePending={archiveChannel.isPending}
        />
      ),
    };
  };
  const rows = ordered.map(conversationOf);
  const rowOf = (row: SidebarConversation, sortable: boolean): JSX.Element => (
    <SidebarConversationRow
      conversation={row}
      selected={row.id === selectedChannelId}
      draggable={sortable}
    />
  );

  if (collapsed) {
    const revealed = collapsedRevealRows(rows, {
      pokeUnread,
      isSelected: (row) => row.id === selectedChannelId,
      isLoud: (row) => row.face.badge?.tier === "loud",
    });
    return (
      <>
        {revealed.length > 0 ? revealCaption : null}
        <SidebarReveal initial={mountedCollapsed.current}>
          {revealed.map((row) => (
            <SidebarRevealRow key={row.id}>{rowOf(row, false)}</SidebarRevealRow>
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

  if (rows.length === 0) {
    return <SidebarEmptyRow>{t("chat.noChannels")}</SidebarEmptyRow>;
  }

  return (
    <SortableGroup
      ids={rows.map((row) => row.id)}
      onReorder={(next) => onReorder("channels", next)}
    >
      {rows.map((row) => (
        <SortableChannelItem key={row.id} id={row.id}>
          {rowOf(row, true)}
        </SortableChannelItem>
      ))}
    </SortableGroup>
  );
}

function SortableChannelItem({ id, children }: { id: string; children: ReactNode }): JSX.Element {
  const { setNodeRef, transform, transition, isDragging, attributes, listeners } = useSortable({
    id,
    animateLayoutChanges,
  });
  const chrome = useSortableRowChrome({ transform, transition, isDragging });
  return (
    <li ref={setNodeRef} {...chrome} className="touch-manipulation" {...attributes} {...listeners}>
      {children}
    </li>
  );
}

export type { ChannelNode };
