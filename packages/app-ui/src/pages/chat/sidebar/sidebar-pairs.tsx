import { createContext, useContext, type MouseEvent, type PointerEvent } from "react";
import { useDndMonitor } from "@dnd-kit/core";
import {
  armConversationDrag,
  useRowLiftedFor,
  beginConversationDrag,
  endConversationDrag,
} from "@/components/layout/pane-drop";
import { AppContextMenuItem, AppContextMenuSeparator } from "@/components/menus/app-context-menu";
import type { ConversationPair } from "@/features/conversation-shell/conversation-pairs";
import { SplitRightIcon } from "@/icons";
import { t } from "@/lib/i18n";

/**
 * Conversation pairs as the sidebar sees them, shared by every row source (channel
 * tree, STARRED, DIRECT MESSAGES, Recent) without threading it through each
 * list. Absent (null) where the sidebar cannot open panes: compact layouts.
 */
export interface SidebarPairs {
  /** Every pair the viewer keeps. */
  pairs: readonly ConversationPair[];
  /** The pair on screen, if one is open. */
  active: ConversationPair | null;
  /** Whether a conversation is open, so another can open beside it. */
  canOpen: boolean;
  /** The route's conversation; it cannot also open beside itself. */
  openId: string | null;
  openBeside: (conversationId: string) => void;
  /** Reopen a pair; `workspaceId` is the left conversation's (null: a direct pair). */
  openPair: (pair: ConversationPair, workspaceId: string | null) => void;
  /** Remove one half of a pair; closes that half when the pair is on screen. */
  closeHalf: (pair: ConversationPair, half: "left" | "right") => void;
}

export const SidebarPairsContext = createContext<SidebarPairs | null>(null);

export function useSidebarPairs(): SidebarPairs | null {
  return useContext(SidebarPairsContext);
}

/**
 * What every conversation row does about panes, so the four row sources
 * cannot drift: ⌥-click opens the row beside the open conversation, and the
 * row menu offers "Open on right" — except on the conversation already open.
 */
export function useRowPaneActions(conversationId: string, selected: boolean) {
  const pairing = useSidebarPairs();
  const canOpen = pairing?.canOpen === true && !selected;

  return {
    /** Wrap a row's own select: ⌥-click opens it beside instead. */
    select:
      (open: () => void) =>
      (event: MouseEvent): void => {
        if (canOpen && event.altKey && !event.metaKey && !event.ctrlKey) {
          pairing!.openBeside(conversationId);
          return;
        }
        open();
      },
    /** The row menu's "Open on right" entry; null when it does not apply. */
    menuItem: canOpen ? (
      <OpenOnRightMenuItem onSelect={() => pairing!.openBeside(conversationId)} />
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
 *  `pane-drop`). Mount inside a `SortableGroup`; `conversationIdOf` returns
 *  null for a row that cannot open beside (a pair row). */
export function PaneDragMonitor({
  conversationIdOf,
}: {
  conversationIdOf: (sortId: string) => string | null;
}): null {
  const pairing = useSidebarPairs();
  useDndMonitor({
    onDragStart: ({ active, activatorEvent }) => {
      const id = conversationIdOf(String(active.id));
      // The sortable `<li>` carries the drag transform; its child is the row.
      const item =
        activatorEvent.target instanceof Element ? activatorEvent.target.closest("li") : null;
      const row = item?.firstElementChild;
      if (
        !pairing?.canOpen ||
        !id ||
        id === pairing.openId ||
        !(row instanceof HTMLElement) ||
        !(activatorEvent instanceof MouseEvent)
      ) {
        return;
      }
      beginConversationDrag(id, row, { x: activatorEvent.clientX, y: activatorEvent.clientY });
    },
    onDragEnd: () => {
      const dropped = endConversationDrag();
      if (dropped) pairing?.openBeside(dropped);
    },
    onDragCancel: () => {
      endConversationDrag();
    },
  });
  return null;
}

/**
 * Drag-to-open-beside for a row that is not in a sortable list (DIRECT MESSAGES,
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
  const pairing = useSidebarPairs();
  const lifted = useRowLiftedFor(conversationId);
  if (!pairing?.canOpen || selected) return {};
  return {
    style: lifted ? { opacity: 0.4 } : undefined,
    onPointerDown: (event) => {
      if (event.button !== 0 || event.pointerType !== "mouse") return;
      armConversationDrag(
        event.nativeEvent,
        event.currentTarget,
        conversationId,
        pairing.openBeside,
      );
    },
  };
}
