import { createContext, useContext, type MouseEvent } from "react";
import type { SplitPair } from "@/features/conversation-shell/split-pairs";

/**
 * The split view as the sidebar sees it, shared by every row source (channel
 * tree, STARRED, DIRECT MESSAGES) without threading it through each list.
 * Absent (null) where the sidebar cannot open columns: compact layouts and
 * the hover peek.
 */
export interface SidebarSplit {
  /** Every pair the viewer keeps. */
  pairs: readonly SplitPair[];
  /** The pair on screen, if a split view is open. */
  active: SplitPair | null;
  /** Whether a conversation is open, so a second column can join it. */
  canOpen: boolean;
  openInSplit: (conversationId: string) => void;
  /** Reopen a pair; `workspaceId` is the left conversation's. */
  openPair: (pair: SplitPair, workspaceId: string) => void;
  /** Remove one half of a pair; closes that column when the pair is on screen. */
  closeHalf: (pair: SplitPair, half: "left" | "right") => void;
}

export const SidebarSplitContext = createContext<SidebarSplit | null>(null);

export function useSidebarSplit(): SidebarSplit | null {
  return useContext(SidebarSplitContext);
}

/** ⌥-click opens a row beside the current conversation instead of replacing it. */
export function wantsSplit(event: MouseEvent, split: SidebarSplit | null): boolean {
  return event.altKey && !event.metaKey && !event.ctrlKey && split?.canOpen === true;
}

/** The pair a row belongs to: where it renders (`left`), or where it is folded
 *  into another row (`right`). */
export function rowPairRole(
  pairs: readonly SplitPair[],
  conversationId: string,
): { pair: SplitPair; role: "left" | "right" } | null {
  for (const pair of pairs) {
    if (pair.left === conversationId) return { pair, role: "left" };
    if (pair.right === conversationId) return { pair, role: "right" };
  }
  return null;
}
