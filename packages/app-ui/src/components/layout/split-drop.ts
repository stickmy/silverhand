import { useSyncExternalStore } from "react";

/**
 * Dragging a sidebar row onto the right half of the conversation stage opens it
 * in the second column. The sidebar's sortable drag is confined to the sidebar,
 * so the drop is decided from the pointer, not from a drop zone dnd-kit knows:
 * the stage registers its box, a drag in progress tracks the pointer, and the
 * drop lands when the pointer ends over the right half.
 */
type DropState = "idle" | "dragging" | "over";

let stage: HTMLElement | null = null;
let draggingId: string | null = null;
let state: DropState = "idle";
let droppedThisTick = false;
const listeners = new Set<() => void>();

function setState(next: DropState): void {
  if (next === state) return;
  state = next;
  for (const listener of listeners) listener();
}

function overRightHalf(x: number, y: number): boolean {
  const rect = stage?.getBoundingClientRect();
  if (!rect || rect.width === 0) return false;
  return x >= rect.left + rect.width / 2 && x <= rect.right && y >= rect.top && y <= rect.bottom;
}

function onPointerMove(event: PointerEvent | MouseEvent): void {
  setState(overRightHalf(event.clientX, event.clientY) ? "over" : "dragging");
}

/** The stage that accepts drops, or null when none is mounted. */
export function registerSplitDropStage(element: HTMLElement | null): void {
  stage = element;
}

/** A sidebar row started dragging. Rows that cannot open in a column pass no id. */
export function beginConversationDrag(conversationId: string): void {
  if (!stage) return;
  draggingId = conversationId;
  window.addEventListener("pointermove", onPointerMove);
  window.addEventListener("mousemove", onPointerMove);
  setState("dragging");
}

/** Ends the drag; returns the conversation to open on the right, if it was dropped there. */
export function endConversationDrag(): string | null {
  const dropped = state === "over" ? draggingId : null;
  if (dropped) {
    droppedThisTick = true;
    queueMicrotask(() => {
      droppedThisTick = false;
    });
  }
  draggingId = null;
  window.removeEventListener("pointermove", onPointerMove);
  window.removeEventListener("mousemove", onPointerMove);
  setState("idle");
  return dropped;
}

/** Whether the drag ending now lands on the stage, so the list must not also
 *  treat it as a reorder. Holds for the rest of the tick the drop ends in,
 *  whichever drag-end listener runs first. */
export function dragLandsOnStage(): boolean {
  return state === "over" || droppedThisTick;
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

const snapshot = (): DropState => state;

export function useSplitDropState(): DropState {
  return useSyncExternalStore(subscribe, snapshot, snapshot);
}
