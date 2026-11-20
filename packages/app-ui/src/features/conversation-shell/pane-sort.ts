import type { PointerEvent as ReactPointerEvent } from "react";
import {
  KeyboardSensor,
  PointerSensor,
  useSensor,
  useSensors,
  type Modifier,
  type PointerSensorOptions,
} from "@dnd-kit/core";
import { sortableKeyboardCoordinates } from "@dnd-kit/sortable";

/** A header press that may become a drag; buttons and links inside keep theirs. */
function paneHeaderOf(target: EventTarget | null): HTMLElement | null {
  if (!(target instanceof Element)) return null;
  const control = target.closest(
    "button, a, input, textarea, [role=button], [contenteditable=true]",
  );
  // The grip is a keyboard handle (role=button), and still drags by pointer.
  if (control && !control.hasAttribute("data-pane-grip")) {
    return null;
  }
  return target.closest<HTMLElement>("[data-side-panel-header], [data-conversation-header]");
}

/** Only a pane's header picks it up: the body selects text and scrolls. */
class PaneHeaderSensor extends PointerSensor {
  static override activators = [
    {
      eventName: "onPointerDown" as const,
      handler: ({ nativeEvent }: ReactPointerEvent, { onActivation }: PointerSensorOptions) => {
        if (!nativeEvent.isPrimary || nativeEvent.button !== 0) return false;
        if (!paneHeaderOf(nativeEvent.target)) return false;
        onActivation?.({ event: nativeEvent });
        return true;
      },
    },
  ];
}

/** Pointer from the header past a few pixels, so a click on the title never
 *  reorders; keyboard from the header's grip. */
export function usePaneSortSensors() {
  return useSensors(
    useSensor(PaneHeaderSensor, { activationConstraint: { distance: 6 } }),
    useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates }),
  );
}

/** A held pane moves within the attached panes only: the route's conversation
 *  holds the first place, so nothing is dragged over it. */
export const restrictToAttachedPanes: Modifier = ({
  transform,
  draggingNodeRect,
  activeNodeRect,
}) => {
  const node = activeNodeRect && draggingNodeRect;
  if (!node) return transform;
  const row = document.querySelector<HTMLElement>("[data-pane-row]");
  const primary = row?.querySelector<HTMLElement>("[data-pane-primary]");
  if (!row || !primary) return transform;
  const gap = parseFloat(getComputedStyle(row).columnGap) || 0;
  const minLeft = primary.getBoundingClientRect().right + gap;
  const maxRight = row.getBoundingClientRect().right;
  const x = Math.min(
    Math.max(transform.x, minLeft - activeNodeRect.left),
    maxRight - activeNodeRect.right,
  );
  return { ...transform, x };
};
