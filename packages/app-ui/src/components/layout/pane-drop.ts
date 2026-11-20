import { useSyncExternalStore } from "react";
import { PANE_BG } from "./pane-chrome";

/**
 * Dragging a sidebar row onto the conversation area opens it in the second
 * column. The sidebar's sortable drag is confined to its list, so the drop is
 * decided from the pointer, not from a drop zone dnd-kit knows: the stage
 * registers its columns, a drag in progress tracks the pointer, and the drop
 * lands when the pointer ends over the target column.
 *
 * States:
 * - "pending": a row is held but has not moved far enough sideways; inside a
 *   sortable list this is a reorder, so nothing else shows.
 * - "lifted": the pointer moved past `LIFT_PX` to the right (or left the
 *   sidebar). The row is out of the list: a ghost follows the pointer and the
 *   target column shows; the list stops reordering.
 * - "over": lifted, with the pointer on the target column.
 */
type DropState = "idle" | "pending" | "lifted" | "over";

interface Drag {
  id: string;
  /** A copy of the held row: the ghost IS the row, same width and paint. */
  row: HTMLElement;
  startX: number;
  /** Where in the row the pointer grabbed it, so the ghost lifts in place. */
  grabX: number;
  grabY: number;
}

const LIFT_PX = 24;

let stage: HTMLElement | null = null;
let drag: Drag | null = null;
let pointer = { x: 0, y: 0 };
let state: DropState = "idle";
let leftListThisTick = false;
const listeners = new Set<() => void>();

function emit(): void {
  for (const listener of listeners) listener();
}

function setState(next: DropState): void {
  if (next === state) return;
  state = next;
  emit();
}

/**
 * Where the dropped conversation will stand: over the open conversation pane
 * (the drop opens beside it), else the right half of the conversation area,
 * full height, so the target previews the layout the drop produces.
 */
export function paneDropTarget(): DOMRect | null {
  const row = stage?.querySelector<HTMLElement>("[data-pane-row]");
  if (!row) return null;
  const pane = row.querySelector<HTMLElement>(
    ':scope > [data-pane-slot] > [data-attached-pane="conversation"]',
  );
  const box = (pane ?? row).getBoundingClientRect();
  if (box.width === 0) return null;
  const left = pane ? box.left : box.left + box.width / 2;
  return new DOMRect(left, box.top, box.right - left, box.height);
}

function stateAt(x: number, y: number): DropState {
  if (!drag || !stage) return "idle";
  const stageLeft = stage.getBoundingClientRect().left;
  if (x - drag.startX <= LIFT_PX && x < stageLeft) return "pending";
  const target = paneDropTarget();
  const over =
    target !== null &&
    x >= target.left &&
    x <= target.right &&
    y >= target.top &&
    y <= target.bottom;
  return over ? "over" : "lifted";
}

function onPointerMove(event: PointerEvent | MouseEvent): void {
  pointer = { x: event.clientX, y: event.clientY };
  setState(stateAt(event.clientX, event.clientY));
  for (const listener of pointerListeners) listener();
}

/** The conversation area that accepts drops, or null when none is mounted. */
export function registerPaneDropStage(element: HTMLElement | null): void {
  stage = element;
}

/**
 * A row is held at `at`. Returns false when no conversation area can take the
 * drop.
 */
export function beginConversationDrag(
  conversationId: string,
  row: HTMLElement,
  at: { x: number; y: number },
): boolean {
  if (!stage) return false;
  const box = row.getBoundingClientRect();
  const copy = row.cloneNode(true) as HTMLElement;
  copy.style.width = `${box.width}px`;
  copy.style.height = `${box.height}px`;
  copy.style.background = PANE_BG;
  drag = {
    id: conversationId,
    row: copy,
    startX: at.x,
    grabX: at.x - box.left,
    grabY: at.y - box.top,
  };
  pointer = at;
  window.addEventListener("pointermove", onPointerMove);
  window.addEventListener("mousemove", onPointerMove);
  setState("pending");
  return true;
}

/** Ends the drag; returns the conversation to open, if it was dropped on the target. */
export function endConversationDrag(): string | null {
  const dropped = state === "over" ? (drag?.id ?? null) : null;
  if (state === "lifted" || state === "over") {
    leftListThisTick = true;
    queueMicrotask(() => {
      leftListThisTick = false;
    });
  }
  drag = null;
  window.removeEventListener("pointermove", onPointerMove);
  window.removeEventListener("mousemove", onPointerMove);
  setState("idle");
  return dropped;
}

/** Whether the drag ending now had left its list, so the list must not also
 *  treat it as a reorder. Holds for the rest of the tick the drag ends in,
 *  whichever drag-end listener runs first. */
export function dragLeftList(): boolean {
  return state === "lifted" || state === "over" || leftListThisTick;
}

/**
 * A drag from a row that is not in a sortable list. Nothing happens until the
 * pointer lifts the row; a release after that is a drop (or a cancel), never a
 * click on the row.
 */
export function armConversationDrag(
  event: PointerEvent,
  row: HTMLElement,
  conversationId: string,
  onDrop: (conversationId: string) => void,
): void {
  if (!beginConversationDrag(conversationId, row, { x: event.clientX, y: event.clientY })) {
    return;
  }
  // An avatar image would start a native drag and swallow the pointer moves.
  const noNativeDrag = (native: DragEvent): void => native.preventDefault();
  window.addEventListener("dragstart", noNativeDrag);
  const finish = (): void => {
    window.removeEventListener("dragstart", noNativeDrag);
    window.removeEventListener("pointerup", onUp);
    window.removeEventListener("pointercancel", onCancel);
    window.removeEventListener("keydown", onKey);
  };
  const onUp = (): void => {
    finish();
    const wasLifted = state === "lifted" || state === "over";
    const dropped = endConversationDrag();
    if (wasLifted) {
      window.addEventListener("click", swallowClick, { capture: true, once: true });
      setTimeout(() => window.removeEventListener("click", swallowClick, { capture: true }), 0);
    }
    if (dropped) onDrop(dropped);
  };
  const onCancel = (): void => {
    finish();
    endConversationDrag();
  };
  const onKey = (key: KeyboardEvent): void => {
    if (key.key === "Escape") onCancel();
  };
  window.addEventListener("pointerup", onUp);
  window.addEventListener("pointercancel", onCancel);
  window.addEventListener("keydown", onKey);
}

function swallowClick(event: MouseEvent): void {
  event.stopPropagation();
  event.preventDefault();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

const snapshot = (): DropState => state;

export function usePaneDropState(): DropState {
  return useSyncExternalStore(subscribe, snapshot, snapshot);
}

/** Whether a held row has left its list for the conversation area. */
export function useRowLifted(): boolean {
  const current = usePaneDropState();
  return current === "lifted" || current === "over";
}

/** Whether this conversation's row is the one lifted out, so it can fade. */
export function useRowLiftedFor(conversationId: string): boolean {
  return useRowLifted() && drag?.id === conversationId;
}

const pointerListeners = new Set<() => void>();

function subscribePointer(listener: () => void): () => void {
  pointerListeners.add(listener);
  return () => {
    pointerListeners.delete(listener);
  };
}

const pointerSnapshot = () => pointer;

/** The held row's copy and where it sits under the pointer. */
export function usePaneDragGhost(): { row: HTMLElement; left: number; top: number } | null {
  const at = useSyncExternalStore(subscribePointer, pointerSnapshot, pointerSnapshot);
  if (!drag) return null;
  return { row: drag.row, left: at.x - drag.grabX, top: at.y - drag.grabY };
}
