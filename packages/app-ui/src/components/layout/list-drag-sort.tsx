import { useListSortSensors } from "./list-sort-sensors";
import { dragLeftList, useRowLifted } from "./pane-drop";
import type { CSSProperties, ReactNode } from "react";
import { DndContext, closestCenter } from "@dnd-kit/core";
import { restrictToParentElement, restrictToVerticalAxis } from "@dnd-kit/modifiers";
import {
  SortableContext,
  arrayMove,
  defaultAnimateLayoutChanges,
  verticalListSortingStrategy,
  type AnimateLayoutChanges,
} from "@dnd-kit/sortable";

/** Shared motion only. Sidebar drag chrome reuses the selected-row theme rules. */
export function sortableItemStyle(args: {
  transform: { y: number } | null;
  transition: string | undefined;
  isDragging: boolean;
}): CSSProperties {
  return {
    transform: args.transform ? `translate3d(0, ${args.transform.y}px, 0)` : undefined,
    transition: args.transition,
    cursor: args.isDragging ? "grabbing" : undefined,
    zIndex: args.isDragging ? 1 : undefined,
    position: args.isDragging ? "relative" : undefined,
  };
}

/**
 * A sortable row's motion and drag state. While a held row has left the list
 * for the conversation area (split drop), the list stands still: every row
 * returns to its slot and the held one fades, so it reads as taken out rather
 * than still being sorted.
 */
export function useSortableRowChrome(args: {
  transform: { y: number } | null;
  transition: string | undefined;
  isDragging: boolean;
}): { style: CSSProperties; "data-sidebar-sort-dragging": true | undefined } {
  const lifted = useRowLifted();
  if (!lifted) {
    return {
      style: sortableItemStyle(args),
      "data-sidebar-sort-dragging": args.isDragging || undefined,
    };
  }
  return {
    style: { opacity: args.isDragging ? 0.4 : undefined },
    "data-sidebar-sort-dragging": undefined,
  };
}

/** Settle guard: animate sibling rows WHILE sorting (the smooth "make way"
 *  slide), but skip dnd-kit's post-drop settle animation. We reorder the data
 *  synchronously on drop (INVARIANT #1), so the row is already in its final DOM
 *  slot — a settle would then animate a leftover transform→0, the visible
 *  "jump". Pass to every `useSortable`. */
export const animateLayoutChanges: AnimateLayoutChanges = (args) =>
  args.isSorting ? defaultAnimateLayoutChanges(args) : false;

/** Generic per-group DnD context: vertical-only, in-place (no DragOverlay),
 *  4px mouse activation (no touch drag: see `useListSortSensors`). Each hard group gets
 *  its own context so drags never cross a group boundary. The consumer renders
 *  its `SortableXxxItem` rows as children and sends the COMPLETE new group
 *  order up through `onReorder`. */
export function SortableGroup({
  ids,
  onReorder,
  children,
}: {
  ids: string[];
  onReorder: (orderedIds: string[]) => void;
  children: ReactNode;
}): JSX.Element {
  const sensors = useListSortSensors();
  return (
    <DndContext
      sensors={sensors}
      collisionDetection={closestCenter}
      modifiers={[restrictToVerticalAxis, restrictToParentElement]}
      onDragEnd={({ active, over }) => {
        if (!over || active.id === over.id || dragLeftList()) return;
        const oldIndex = ids.indexOf(String(active.id));
        const newIndex = ids.indexOf(String(over.id));
        if (oldIndex < 0 || newIndex < 0) return;
        onReorder(arrayMove(ids, oldIndex, newIndex));
      }}
    >
      <SortableContext items={ids} strategy={verticalListSortingStrategy}>
        {children}
      </SortableContext>
    </DndContext>
  );
}
