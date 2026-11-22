import type { ReactNode } from "react";
import type { RowBadge } from "@/features/attention/attention-mode";
import { AppContextMenu } from "@/components/menus/app-context-menu";
import {
  ConversationRowLayout,
  MutedMarker,
  UnreadBadge,
} from "@/components/layout/sidebar-shared";
import { cn } from "@/lib/cn";

/**
 * A conversation as every sidebar list shows it — a channel in the tree, a
 * STARRED row, a direct line — so one row renders them all and click and menu
 * behave the same everywhere. Each list adapts its own data source into this.
 */
export interface SidebarConversation {
  id: string;
  /** Null for a direct pair, which has no workspace. */
  workspaceId: string | null;
  face: RowFace;
  muted: boolean;
  /** A trailing marker after the title (a deleted counterpart). */
  marker?: ReactNode;
  /** Hover text when the title alone is ambiguous. */
  tooltip?: string;
  /** False for a row that is not a conversation yet (a DM draft): no menu. */
  openable?: boolean;
  open: () => void;
  menu: ReactNode;
}

export interface RowFace {
  glyph: ReactNode;
  title: string;
  badge: RowBadge | null;
}

/** The one sidebar conversation row: the conversation with its menu. */
export function SidebarConversationRow({
  conversation,
  selected,
  draggable = false,
}: {
  conversation: SidebarConversation;
  selected: boolean;
  /** Paint the grab cursor: the row reorders in its list. */
  draggable?: boolean;
}): JSX.Element {
  const { face } = conversation;
  const row = (
    <ConversationRowLayout
      selected={selected}
      column="nested"
      draggable={draggable}
      title={conversation.tooltip}
      onSelect={conversation.open}
    >
      <div className="flex size-5.5 shrink-0 items-center justify-center">{face.glyph}</div>
      <span className={cn("min-w-0 truncate", selected && "font-medium")}>{face.title}</span>
      {conversation.marker ? (
        <span className="flex shrink-0 items-center">{conversation.marker}</span>
      ) : null}
      {conversation.muted ? (
        <span className="flex shrink-0 items-center">
          <MutedMarker />
        </span>
      ) : null}
      {face.badge ? (
        <UnreadBadge count={face.badge.count} tier={face.badge.tier} className="ml-auto" />
      ) : null}
    </ConversationRowLayout>
  );
  if (conversation.openable === false) return row;
  return <AppContextMenu trigger={row}>{conversation.menu}</AppContextMenu>;
}
