import { useLayoutEffect, useSyncExternalStore, type ReactNode } from "react";
import type { RowBadge } from "@/features/attention/attention-mode";
import type { ConversationPair } from "@/features/conversation-shell/conversation-pairs";
import { AppContextMenu } from "@/components/menus/app-context-menu";
import {
  ConversationRowLayout,
  MutedMarker,
  UnreadBadge,
} from "@/components/layout/sidebar-shared";
import { cn } from "@/lib/cn";
import { useConversationDragSource, useRowPaneActions } from "./sidebar-pairs";
import { SidebarPairRow } from "./pair-row";

/**
 * A conversation as every sidebar list shows it — a channel in the tree, a
 * STARRED row, a direct line — so one row renders them all and click, ⌥-click,
 * menu, drag-to-pane and pair folding behave the same everywhere. Each list
 * adapts its own data source into this.
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
  /** False for a row that is not a conversation yet (a DM draft): no menu,
   *  no ⌥-click, no drag to a pane. */
  openable?: boolean;
  open: () => void;
  /** The row menu's own entries; the shared row adds "Open on right". */
  menu: ReactNode;
}

/**
 * The face each sidebar list gives the conversations it lists, readable by the
 * other lists. A pair can join conversations from two lists (a DM beside a
 * channel), and its row stands where its left conversation is listed, so that
 * list needs the right half's face, and the right half's own list needs to know
 * the pair row took it.
 */
export interface RowFace {
  glyph: ReactNode;
  title: string;
  badge: RowBadge | null;
  /** Changes when `glyph` would draw differently (an avatar landed). */
  glyphKey?: string;
}

const byList = new Map<string, ReadonlyMap<string, RowFace>>();
let merged = new Map<string, RowFace>();
const listeners = new Set<() => void>();

function signature(faces: ReadonlyMap<string, RowFace>): string {
  let out = "";
  for (const [id, face] of faces) {
    out += `${id}\u0000${face.title}\u0000${face.badge?.count ?? ""}\u0000${face.badge?.tier ?? ""}\u0000${face.glyphKey ?? ""}\u0001`;
  }
  return out;
}

const signatures = new Map<string, string>();

function publish(): void {
  merged = new Map();
  for (const faces of byList.values()) {
    for (const [id, face] of faces) merged.set(id, face);
  }
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

const snapshot = () => merged;

/** Publish a list's conversations for as long as it is mounted, and fold the
 *  viewer's kept pairs into its rows. */
export function useSidebarList<T>(
  list: string,
  rows: readonly T[],
  conversationOf: (row: T) => SidebarConversation | null,
  pairs: readonly ConversationPair[],
  /** The conversations other lists may fold into a pair; defaults to all. */
  listed?: (conversation: SidebarConversation) => boolean,
): { lines: FoldedRow<T>[]; byId: ReadonlyMap<string, SidebarConversation> } {
  const byId = new Map<string, SidebarConversation>();
  for (const row of rows) {
    const conversation = conversationOf(row);
    if (conversation) byId.set(conversation.id, conversation);
  }
  const faces = new Map(
    [...byId.values()].filter((c) => listed?.(c) ?? true).map((c) => [c.id, c.face]),
  );
  useRegisterRowFaces(list, faces);
  const all = useRowFaces();
  const lines = foldPairs(rows, (row) => conversationOf(row)?.id ?? "", pairs, all);
  return { lines, byId };
}

/**
 * The one sidebar conversation row: a kept pair when `pair` is set, else the
 * conversation's own row with its menu, ⌥-click to open on the right, and —
 * outside a sortable list, which lifts rows itself — drag to a pane.
 */
export function SidebarConversationRow({
  conversation,
  pair,
  selected,
  draggable = false,
  sortable = false,
}: {
  conversation: SidebarConversation;
  pair: FoldedRow<unknown>["pair"];
  selected: boolean;
  /** Paint the grab cursor: the row reorders in its list. */
  draggable?: boolean;
  /** The row lives in a sortable list, whose drag lifts it to a pane. */
  sortable?: boolean;
}): JSX.Element {
  const openable = conversation.openable !== false;
  const paneActions = useRowPaneActions(conversation.id, selected || !openable);
  const dragSource = useConversationDragSource(conversation.id, selected || !openable || sortable);

  if (pair) {
    return (
      <SidebarPairRow
        pair={pair.pair}
        workspaceId={conversation.workspaceId}
        draggable={draggable}
        left={conversation.face}
        right={pair.right}
        menu={conversation.menu}
      />
    );
  }

  const { face } = conversation;
  const row = (
    <ConversationRowLayout
      {...dragSource}
      selected={selected}
      column="nested"
      draggable={draggable}
      title={conversation.tooltip}
      onSelect={paneActions.select(conversation.open)}
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
  if (!openable) return row;
  return (
    <AppContextMenu trigger={row}>
      {paneActions.menuItem}
      {conversation.menu}
    </AppContextMenu>
  );
}

/** Publish a list's faces for as long as it is mounted. */
function useRegisterRowFaces(list: string, faces: ReadonlyMap<string, RowFace>): void {
  useLayoutEffect(() => {
    const next = signature(faces);
    byList.set(list, faces);
    if (signatures.get(list) !== next) {
      signatures.set(list, next);
      publish();
    }
  });

  useLayoutEffect(
    () => () => {
      byList.delete(list);
      signatures.delete(list);
      publish();
    },
    [list],
  );
}

/** Every listed conversation's face, across the sidebar's lists. */
function useRowFaces(): ReadonlyMap<string, RowFace> {
  return useSyncExternalStore(subscribe, snapshot, snapshot);
}

/** A row in a list that folds kept pairs. */
export type FoldedRow<T> = {
  row: T;
  /** Set on the row a pair takes over: the pair and its right half's face. */
  pair: { pair: ConversationPair; right: RowFace } | null;
};

/**
 * Fold kept pairs into one list's rows. A pair's row stands in its left
 * conversation's slot, wherever that is listed; the right conversation's own
 * row is dropped when the left is listed somewhere. Halves that are not both
 * listed stay plain rows.
 */
export function foldPairs<T>(
  rows: readonly T[],
  idOf: (row: T) => string,
  pairs: readonly ConversationPair[],
  faces: ReadonlyMap<string, RowFace>,
): FoldedRow<T>[] {
  const out: FoldedRow<T>[] = [];
  for (const row of rows) {
    const id = idOf(row);
    const pair = pairs.find((p) => p.left === id || p.right === id);
    const right = pair ? faces.get(pair.right) : undefined;
    const bothListed = pair !== undefined && right !== undefined && faces.has(pair.left);
    if (!bothListed) {
      out.push({ row, pair: null });
    } else if (pair.left === id) {
      out.push({ row, pair: { pair, right } });
    }
  }
  return out;
}

/** Expand a reordered list of folded rows back to every row id: a pair's right
 *  half, when this list holds it, goes right after its left. */
export function unfoldOrder(
  orderedIds: readonly string[],
  allIds: readonly string[],
  pairs: readonly ConversationPair[],
): string[] {
  const listed = new Set(allIds);
  const placed = new Set(orderedIds);
  return orderedIds.flatMap((id) => {
    const pair = pairs.find((p) => p.left === id);
    return pair && listed.has(pair.right) && !placed.has(pair.right) ? [id, pair.right] : [id];
  });
}
