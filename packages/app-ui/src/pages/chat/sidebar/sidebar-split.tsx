import { createContext, useContext, type MouseEvent, type PointerEvent } from "react";
import { useDndMonitor } from "@dnd-kit/core";
import {
  armConversationDrag,
  useRowLiftedFor,
  beginConversationDrag,
  endConversationDrag,
} from "@/components/layout/split-drop";
import { AppContextMenuItem, AppContextMenuSeparator } from "@/components/menus/app-context-menu";
import type { SplitPair } from "@/features/conversation-shell/split-pairs";
import { SplitRightIcon } from "@/icons";
import { t } from "@/lib/i18n";

/**
 * The split view as the sidebar sees it, shared by every row source (channel
 * tree, STARRED, DIRECT MESSAGES, Recent) without threading it through each
 * list. Absent (null) where the sidebar cannot open columns: compact layouts.
 */
export interface SidebarSplit {
  /** Every pair the viewer keeps. */
  pairs: readonly SplitPair[];
  /** The pair on screen, if a split view is open. */
  active: SplitPair | null;
  /** Whether a conversation is open, so a second column can join it. */
  canOpen: boolean;
  /** The conversation open in the first column; it cannot also open beside itself. */
  openId: string | null;
  openInSplit: (conversationId: string) => void;
  /** Reopen a pair; `workspaceId` is the left conversation's (null: a direct pair). */
  openPair: (pair: SplitPair, workspaceId: string | null) => void;
  /** Remove one half of a pair; closes that column when the pair is on screen. */
  closeHalf: (pair: SplitPair, half: "left" | "right") => void;
}

export const SidebarSplitContext = createContext<SidebarSplit | null>(null);

export function useSidebarSplit(): SidebarSplit | null {
  return useContext(SidebarSplitContext);
}

/**
 * What every conversation row does about split view, so the four row sources
 * cannot drift: ⌥-click opens the row beside the open conversation, and the
 * row menu offers "Open on right" — except on the conversation already open.
 */
export function useRowSplitActions(conversationId: string, selected: boolean) {
  const split = useSidebarSplit();
  const canOpen = split?.canOpen === true && !selected;

  return {
    /** Wrap a row's own select: ⌥-click goes to the second column instead. */
    select:
      (open: () => void) =>
      (event: MouseEvent): void => {
        if (canOpen && event.altKey && !event.metaKey && !event.ctrlKey) {
          split!.openInSplit(conversationId);
          return;
        }
        open();
      },
    /** The row menu's "Open on right" entry; null when it does not apply. */
    menuItem: canOpen ? (
      <OpenOnRightMenuItem onSelect={() => split!.openInSplit(conversationId)} />
    ) : null,
  };
}

function OpenOnRightMenuItem({ onSelect }: { onSelect: () => void }): JSX.Element {
  return (
    <>
      <AppContextMenuItem
        icon={<SplitRightIcon className="size-3.5" />}
        trailing={t("conversationShell.openOnRightHint")}
        onSelect={onSelect}
      >
        {t("conversationShell.openOnRight")}
      </AppContextMenuItem>
      <AppContextMenuSeparator />
    </>
  );
}

/** Lets a sortable row drag out of its list onto the conversation area. Within
 *  the list a drag reorders; moving it far enough sideways lifts it out (see
 *  `split-drop`). Mount inside a `SortableGroup`; `conversationIdOf` returns
 *  null for a row that cannot open in a column (a pair row). */
export function SplitDragMonitor({
  conversationIdOf,
}: {
  conversationIdOf: (sortId: string) => string | null;
}): null {
  const split = useSidebarSplit();
  useDndMonitor({
    onDragStart: ({ active, activatorEvent }) => {
      const id = conversationIdOf(String(active.id));
      // The sortable `<li>` carries the drag transform; its child is the row.
      const item =
        activatorEvent.target instanceof Element ? activatorEvent.target.closest("li") : null;
      const row = item?.firstElementChild;
      if (
        !split?.canOpen ||
        !id ||
        id === split.openId ||
        !(row instanceof HTMLElement) ||
        !(activatorEvent instanceof MouseEvent)
      ) {
        return;
      }
      beginConversationDrag(id, row, { x: activatorEvent.clientX, y: activatorEvent.clientY });
    },
    onDragEnd: () => {
      const dropped = endConversationDrag();
      if (dropped) split?.openInSplit(dropped);
    },
    onDragCancel: () => {
      endConversationDrag();
    },
  });
  return null;
}

/**
 * Drag-to-split for a row that is not in a sortable list (DIRECT MESSAGES,
 * Recent): spread the returned handler on the row. Mouse only; on touch a long
 * press belongs to the row menu.
 */
export function useConversationDragSource(
  conversationId: string,
  selected: boolean,
): {
  onPointerDown?: (event: PointerEvent<HTMLElement>) => void;
  style?: { opacity: number };
} {
  const split = useSidebarSplit();
  const lifted = useRowLiftedFor(conversationId);
  if (!split?.canOpen || selected) return {};
  return {
    style: lifted ? { opacity: 0.4 } : undefined,
    onPointerDown: (event) => {
      if (event.button !== 0 || event.pointerType !== "mouse") return;
      armConversationDrag(
        event.nativeEvent,
        event.currentTarget,
        conversationId,
        split.openInSplit,
      );
    },
  };
}
