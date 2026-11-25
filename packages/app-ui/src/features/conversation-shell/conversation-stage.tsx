import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  type PointerEvent as ReactPointerEvent,
  type PointerEventHandler,
  type ReactNode,
} from "react";
import { useReducedMotion } from "motion/react";
import { DndContext, closestCenter } from "@dnd-kit/core";
import { restrictToHorizontalAxis } from "@dnd-kit/modifiers";
import { SortableContext, horizontalListSortingStrategy, useSortable } from "@dnd-kit/sortable";
import { animateLayoutChanges } from "@/components/layout/list-drag-sort";
import { useCompactLayout } from "@/navigation/layout";
import { paneKey, type Pane } from "@/navigation/panes";
import { PANE_BG } from "@/components/layout/pane-chrome";
import { PaneMovableProvider, type PaneMovable } from "@/components/layout/side-panel-header";
import { SplitterTrack } from "@/components/layout/splitter-track";
import { useResizableWidth } from "@/hooks/use-resizable-width";
import { ConversationViewSkeleton } from "@/features/conversation-shell/components/conversation-view-skeleton";
import { cn } from "@/lib/cn";
import { AttachedPane } from "./attached-pane";
import type { ConversationContent } from "./conversation-content";
import { PrimaryPaneContent } from "./primary-pane-content";
import { restrictToAttachedPanes, usePaneSortSensors } from "./pane-sort";

export function ConversationStage({
  content,
  ready,
  storageKey,
}: {
  content: ConversationContent;
  ready: boolean;
  storageKey: string;
}) {
  const compact = useCompactLayout();
  const sensors = usePaneSortSensors();
  const keys = useMemo(() => content.panes.map(paneKey), [content.panes]);
  const mounted = useMountOrder(keys);
  if (!ready) {
    return <ConversationViewSkeleton showTabs={content.loadingTabs} />;
  }
  const last = content.panes.at(-1);
  if (compact) {
    return last ? (
      <AttachedPane key={paneKey(last)} pane={last} ctx={content.ctx} />
    ) : (
      <PrimaryPaneContent model={content.primary} />
    );
  }
  // The route's conversation stays mounted as panes open and close beside it,
  // so its scroll position and draft survive.
  return (
    <div
      data-pane-row
      className="flex h-full min-w-0 flex-1 overflow-hidden"
      style={{ gap: "var(--pane-gap)" }}
    >
      <div
        data-pane-primary
        className="order-first flex h-full flex-1 overflow-hidden"
        style={{ minWidth: PRIMARY_MIN_WIDTH }}
      >
        <PrimaryPaneContent model={content.primary} />
      </div>
      <DndContext
        sensors={sensors}
        collisionDetection={closestCenter}
        modifiers={[restrictToHorizontalAxis, restrictToAttachedPanes]}
        onDragStart={() => {
          // The press began a text selection; a drag must not extend it.
          window.getSelection()?.removeAllRanges();
          document.body.style.cursor = "grabbing";
          document.body.style.userSelect = "none";
        }}
        onDragEnd={({ active, over }) => {
          endPaneDrag();
          if (!over || active.id === over.id) return;
          const to = keys.indexOf(String(over.id));
          if (to >= 0) content.ctx.commands.movePane(String(active.id), to);
        }}
        onDragCancel={endPaneDrag}
      >
        <SortableContext items={keys} strategy={horizontalListSortingStrategy}>
          {mounted.map((key) => {
            const pane = content.panes.find((p) => paneKey(p) === key)!;
            return (
              <PaneSlot key={key} pane={pane} storageKey={storageKey} order={keys.indexOf(key)}>
                <AttachedPane pane={pane} ctx={content.ctx} />
              </PaneSlot>
            );
          })}
        </SortableContext>
      </DndContext>
    </div>
  );
}

function endPaneDrag(): void {
  document.body.style.cursor = "";
  document.body.style.userSelect = "";
}

/** The panes in the order they first opened. Moving a node in the DOM resets
 *  every scroller inside it, so the row reorders with CSS `order` instead and
 *  the DOM keeps this order. */
function useMountOrder(keys: readonly string[]): string[] {
  const order = useRef<string[]>([]);
  const kept = order.current.filter((key) => keys.includes(key));
  const added = keys.filter((key) => !kept.includes(key));
  order.current = [...kept, ...added];
  return order.current;
}

const PRIMARY_MIN_WIDTH = 320;

/** Fired on the pane row when any pane starts a resize. */
const RESIZE_START = "pane-resize-start";
/** Fired on an attached pane whose right-hand neighbour's splitter is moving. */
const NEIGHBOUR_RESIZE = "pane-neighbour-resize";

// No own ceiling: a pane grows until the primary column reaches its minimum.
const PANE_WIDTH = { defaultWidth: 400, min: 300, max: Number.POSITIVE_INFINITY } as const;

/** An attached pane at its own width, resized from its left edge. */
function PaneSlot({
  pane,
  storageKey,
  order,
  children,
}: {
  pane: Pane;
  storageKey: string;
  /** Place in the row. */
  order: number;
  children: ReactNode;
}): JSX.Element {
  const clip = useRef<HTMLDivElement | null>(null);
  // The pane on the splitter's other side, and the widths both started at.
  const drag = useRef<{ left: HTMLElement | null; leftStart: number; ownStart: number } | null>(
    null,
  );
  const bounds = useCallback(() => {
    const own = clip.current;
    const row = own?.closest("[data-pane-row]");
    const primary = row?.querySelector("[data-pane-primary]");
    if (!own || !row || !primary) return null;
    const width = own.getBoundingClientRect().width;
    const left = drag.current?.left;
    if (left) {
      // Between two attached panes the splitter trades width between those two only.
      const leftWidth = left.getBoundingClientRect().width;
      return {
        width,
        min: width - (PANE_WIDTH.max - leftWidth),
        max: width + leftWidth - PANE_WIDTH.min,
      };
    }
    return { width, max: width + primary.getBoundingClientRect().width - PRIMARY_MIN_WIDTH };
  }, []);
  const onDrag = useCallback((next: number, done: boolean) => {
    const current = drag.current;
    if (!current?.left) return;
    const detail = { width: current.leftStart + current.ownStart - next, done };
    current.left.dispatchEvent(new CustomEvent(NEIGHBOUR_RESIZE, { detail }));
    if (done) drag.current = null;
  }, []);
  const {
    width,
    startResize,
    isDragging: resizing,
    pin,
  } = useResizableWidth({
    storageKey: `${storageKey}.pane.${pane.kind}`,
    ...PANE_WIDTH,
    edge: "left",
    bounds,
    onDrag,
  });

  // When the row is narrower than the panes' widths, flex shrinks them all
  // together, so changing one pane's width would redistribute the others.
  // Every pane takes its laid-out width when a resize starts; from then on
  // only the two panes beside the splitter move.
  useEffect(() => {
    const own = clip.current;
    const row = own?.closest("[data-pane-row]");
    if (!own || !row) return;
    const onStart = (): void => {
      const laid = own.getBoundingClientRect().width;
      if (laid) pin(laid);
    };
    const onNeighbour = (e: Event): void => {
      const { width: next, done } = (e as CustomEvent<{ width: number; done: boolean }>).detail;
      pin(next, done);
    };
    row.addEventListener(RESIZE_START, onStart);
    own.addEventListener(NEIGHBOUR_RESIZE, onNeighbour);
    return () => {
      row.removeEventListener(RESIZE_START, onStart);
      own.removeEventListener(NEIGHBOUR_RESIZE, onNeighbour);
    };
  }, [pin]);
  const beginResize = useCallback(
    (e: ReactPointerEvent) => {
      const own = clip.current;
      const row = own?.closest("[data-pane-row]");
      if (!own || !row) return;
      row.dispatchEvent(new Event(RESIZE_START));
      const left =
        order > 0 ? row.querySelector<HTMLElement>(`[data-pane-order="${order - 1}"]`) : null;
      drag.current = {
        left,
        leftStart: left?.getBoundingClientRect().width ?? 0,
        ownStart: own.getBoundingClientRect().width,
      };
      startResize(e);
    },
    [order, startResize],
  );

  const reducedMotion = useReducedMotion();
  const sortable = useSortable({
    id: paneKey(pane),
    animateLayoutChanges,
    transition: reducedMotion ? null : { duration: 200, easing: "cubic-bezier(0.2, 0, 0, 1)" },
  });
  const { attributes, listeners, setActivatorNodeRef, transform, isDragging } = sortable;
  const grip = useMemo(
    () => ({
      ref: setActivatorNodeRef,
      attributes,
      onKeyDown: listeners?.onKeyDown as PaneMovable["onKeyDown"],
    }),
    [setActivatorNodeRef, attributes, listeners],
  );

  // The held pane itself follows the pointer, so it is the pane in every
  // respect: width, scroll position, content.
  return (
    <div
      ref={sortable.setNodeRef}
      data-pane-slot={paneKey(pane)}
      className="relative flex h-full min-w-0 shrink"
      style={{
        order,
        gap: "var(--pane-gap)",
        transform: transform ? `translate3d(${transform.x}px, 0, 0)` : undefined,
        transition: sortable.transition,
        zIndex: isDragging ? 1 : undefined,
      }}
    >
      <SplitterTrack onPointerDown={beginResize} active={resizing} />
      <div
        ref={clip}
        data-pane-clip="side"
        data-attached-pane={pane.kind}
        data-pane-order={order}
        onPointerDown={listeners?.onPointerDown as PointerEventHandler | undefined}
        className={cn(
          "flex h-full min-w-0 shrink overflow-hidden [&_[data-conversation-header]]:cursor-grab [&_[data-side-panel-header]]:cursor-grab",
          isDragging && "shadow-lg",
        )}
        style={{ width, contain: "layout paint", background: PANE_BG }}
      >
        <PaneMovableProvider value={grip}>{children}</PaneMovableProvider>
      </div>
    </div>
  );
}
