import {
  useCallback,
  useEffect,
  useImperativeHandle,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  startTransition,
  type MouseEvent,
  type ReactNode,
  type Ref,
} from "react";
import { cn } from "@/lib/cn";

const ANCHOR_ROWS = 4;
// Rows mounted in the first frame, around the initial anchor: enough to fill
// a tall viewport. The rest of the window mounts in the following frames.
const FIRST_PAINT_ROWS = 24;

export interface MessageScrollerHandle {
  scrollToBottom(): void;
  /** False when the row is not in the loaded window. */
  scrollToKey(key: string, align: "start" | "center"): boolean;
}

export type MessageScrollerAnchor =
  | { kind: "bottom" }
  | { kind: "key"; key: string; align: "start" | "center" };

/**
 * The message list's scroll engine. Every loaded row is real DOM, so heights
 * come from layout and are never estimated. The reader's place is held as
 * (first visible row, its offset); any change above it — older pages
 * prepended, an image loading, an edit — is compensated before paint, so rows
 * never visibly move. A reader at the bottom stays at the bottom instead.
 *
 * `MessageList` depends only on this props/handle contract, so the engine can
 * be swapped (back to a virtualizer, say) without touching its callers.
 */
export function MessageScroller<T>({
  items,
  itemKey,
  renderItem,
  header,
  footer,
  initialAnchor,
  follow,
  atBottomThreshold,
  reachEdgeDistance,
  onReachTop,
  onReachBottom,
  onAtBottomChange,
  onTopItemChange,
  controlRef,
  className,
}: {
  items: readonly T[];
  itemKey: (item: T) => string;
  renderItem: (item: T) => ReactNode;
  header?: ReactNode;
  footer?: ReactNode;
  /** Read once, when the engine mounts. */
  initialAnchor: MessageScrollerAnchor;
  /** Whether a reader at the bottom is kept there as content grows. */
  follow: boolean;
  atBottomThreshold: number;
  /** How close to either end the next page is requested. */
  reachEdgeDistance: number;
  onReachTop: () => void;
  onReachBottom: () => void;
  onAtBottomChange: (atBottom: boolean) => void;
  onTopItemChange: (key: string) => void;
  controlRef: Ref<MessageScrollerHandle>;
  className?: string;
}): JSX.Element {
  const scrollerRef = useRef<HTMLDivElement>(null);
  const contentRef = useRef<HTMLDivElement>(null);
  const rowsRef = useRef<HTMLDivElement>(null);
  // The first visible rows with their offsets. Several, because the top one
  // can change key when older rows merge into it (an activity cluster).
  const anchorRef = useRef<{ key: string; offset: number }[]>([]);
  const atBottomRef = useRef<boolean | null>(null);
  const lastScrollTopRef = useRef(0);
  // Content height at the last settle. When it differs, the DOM changed size
  // since the anchor was taken, and the anchor must be applied, not re-read.
  const settledHeightRef = useRef(0);
  // A disclosure the reader just toggled. Its growth must extend downward from
  // where they are reading, even at the bottom, instead of being pinned under
  // the composer; collapsing must not leave it out of view.
  const toggledRef = useRef<HTMLElement | null>(null);
  const latest = useRef({ follow, atBottomThreshold, reachEdgeDistance });
  latest.current = { follow, atBottomThreshold, reachEdgeDistance };
  const callbacks = useRef({ onReachTop, onReachBottom, onAtBottomChange, onTopItemChange });
  callbacks.current = { onReachTop, onReachBottom, onAtBottomChange, onTopItemChange };
  const initialAnchorRef = useRef(initialAnchor);

  // Mounting a whole window (100+ rows of markdown) costs several hundred ms,
  // so a fresh mount paints only the rows around its anchor and adds the rest
  // a frame later. The added rows above are absorbed by the anchor like a
  // prepend, so nothing moves.
  const [firstPaint, setFirstPaint] = useState(true);
  const rendered = useMemo(() => {
    if (!firstPaint || items.length <= FIRST_PAINT_ROWS) {
      return items;
    }
    const anchor = initialAnchorRef.current;
    const index =
      anchor.kind === "key" ? items.findIndex((item) => itemKey(item) === anchor.key) : -1;
    const end = index < 0 ? items.length : Math.min(items.length, index + FIRST_PAINT_ROWS / 2);
    return items.slice(Math.max(0, end - FIRST_PAINT_ROWS), end);
  }, [items, itemKey, firstPaint]);
  const partial = rendered.length < items.length;
  const partialRef = useRef(partial);
  partialRef.current = partial;
  useEffect(() => {
    const frame = requestAnimationFrame(() => startTransition(() => setFirstPaint(false)));
    return () => cancelAnimationFrame(frame);
  }, []);

  const findRow = useCallback((key: string): HTMLElement | null => {
    const rows = rowsRef.current;
    if (!rows) {
      return null;
    }
    return rows.querySelector<HTMLElement>(`:scope > [data-scroller-key="${CSS.escape(key)}"]`);
  }, []);

  // A hidden scroller (display: none) reports zero geometry; reading it would
  // lose the reader's place, so every pass skips it.
  const visibleScroller = useCallback((): HTMLDivElement | null => {
    const scroller = scrollerRef.current;
    return scroller && scroller.clientHeight > 0 ? scroller : null;
  }, []);

  const captureAnchor = useCallback(() => {
    const scroller = visibleScroller();
    const rows = rowsRef.current;
    if (!scroller || !rows) {
      return;
    }
    const top = scroller.getBoundingClientRect().top;
    const children = rows.children;
    let low = 0;
    let high = children.length - 1;
    let found: HTMLElement | null = null;
    while (low <= high) {
      const mid = (low + high) >> 1;
      const row = children[mid] as HTMLElement;
      if (row.getBoundingClientRect().bottom > top) {
        found = row;
        high = mid - 1;
      } else {
        low = mid + 1;
      }
    }
    const anchors: { key: string; offset: number }[] = [];
    for (let row = found; row && anchors.length < ANCHOR_ROWS;) {
      anchors.push({
        key: row.dataset.scrollerKey!,
        offset: row.getBoundingClientRect().top - top,
      });
      row = row.nextElementSibling as HTMLElement | null;
    }
    anchorRef.current = anchors;
  }, [visibleScroller]);

  const sync = useCallback(() => {
    const scroller = visibleScroller();
    if (!scroller) {
      return;
    }
    const previousKey = anchorRef.current[0]?.key;
    captureAnchor();
    const key = anchorRef.current[0]?.key;
    if (key && key !== previousKey) {
      callbacks.current.onTopItemChange(key);
    }

    // A partial first paint has artificial ends: neither the bottom nor an
    // edge of the window has been reached yet.
    if (partialRef.current) {
      settledHeightRef.current = scroller.scrollHeight;
      lastScrollTopRef.current = scroller.scrollTop;
      return;
    }

    // Only scrolling up leaves the bottom. Content that grew since the last
    // settle (an image, the next row) would otherwise read as the reader
    // having left, and the bottom would stop being held.
    const distance = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight;
    const scrolledUp = scroller.scrollTop < lastScrollTopRef.current;
    lastScrollTopRef.current = scroller.scrollTop;
    const atBottom =
      distance <= latest.current.atBottomThreshold || (atBottomRef.current === true && !scrolledUp);
    if (atBottom !== atBottomRef.current) {
      atBottomRef.current = atBottom;
      callbacks.current.onAtBottomChange(atBottom);
    }

    settledHeightRef.current = scroller.scrollHeight;
    if (scroller.scrollTop <= latest.current.reachEdgeDistance) {
      callbacks.current.onReachTop();
    }
    if (distance <= latest.current.reachEdgeDistance) {
      callbacks.current.onReachBottom();
    }
  }, [captureAnchor, visibleScroller]);

  // Re-places the reader after content or viewport changed size. Runs before
  // paint (layout effect, ResizeObserver), so the intermediate layout is never
  // shown.
  const settle = useCallback(() => {
    const scroller = visibleScroller();
    if (!scroller) {
      return;
    }
    const toggled = toggledRef.current;
    if (toggled && scroller.scrollHeight !== settledHeightRef.current) {
      toggledRef.current = null;
      // Re-derived from the new geometry: grown content has left the bottom.
      atBottomRef.current = null;
    }
    if (latest.current.follow && atBottomRef.current) {
      scroller.scrollTop = scroller.scrollHeight;
    } else {
      for (const anchor of anchorRef.current) {
        const row = findRow(anchor.key);
        if (!row) {
          continue;
        }
        const offset = row.getBoundingClientRect().top - scroller.getBoundingClientRect().top;
        const delta = offset - anchor.offset;
        if (delta !== 0) {
          scroller.scrollTop += delta;
        }
        break;
      }
    }
    if (toggled && toggledRef.current === null) {
      const top = scroller.getBoundingClientRect().top;
      const rect = toggled.getBoundingClientRect();
      if (rect.bottom <= top) {
        scroller.scrollTop += rect.top - top;
      }
    }
    sync();
  }, [findRow, sync, visibleScroller]);

  const handleClickCapture = useCallback(
    (event: MouseEvent) => {
      const toggle = (event.target as Element).closest<HTMLElement>("[aria-expanded]");
      if (!toggle || toggle.hasAttribute("aria-haspopup")) {
        return;
      }
      captureAnchor();
      toggledRef.current = toggle;
      // A toggle that changed nothing must not hold over to the next resize
      // (a live append at the bottom). A real toggle is settled by the
      // ResizeObserver of the next frame, which runs after its rAF callbacks.
      requestAnimationFrame(() =>
        requestAnimationFrame(() => {
          if (toggledRef.current === toggle) {
            toggledRef.current = null;
          }
        }),
      );
    },
    [captureAnchor],
  );

  // A scroll event can land between a size change and its ResizeObserver
  // pass; re-reading the anchor there would adopt the moved layout.
  const handleScroll = useCallback(() => {
    const scroller = visibleScroller();
    if (scroller && scroller.scrollHeight !== settledHeightRef.current) {
      settle();
    } else {
      sync();
    }
  }, [settle, sync, visibleScroller]);

  const scrollToKey = useCallback(
    (key: string, align: "start" | "center"): boolean => {
      const scroller = scrollerRef.current;
      const row = findRow(key);
      if (!scroller || !row) {
        return false;
      }
      const rowRect = row.getBoundingClientRect();
      const offset =
        align === "center" ? Math.max(0, (scroller.clientHeight - rowRect.height) / 2) : 0;
      scroller.scrollTop += rowRect.top - scroller.getBoundingClientRect().top - offset;
      sync();
      return true;
    },
    [findRow, sync],
  );

  useImperativeHandle(
    controlRef,
    () => ({
      scrollToBottom: () => {
        const scroller = scrollerRef.current;
        if (!scroller) {
          return;
        }
        scroller.scrollTop = scroller.scrollHeight;
        sync();
      },
      scrollToKey,
    }),
    [scrollToKey, sync],
  );

  // The anchor must describe the layout BEFORE the new rows commit. Scrolling
  // since the last scroll event is not yet reflected in it, so it is read here,
  // while the DOM still holds the previous rows.
  const renderedItemsRef = useRef(rendered);
  if (renderedItemsRef.current !== rendered) {
    renderedItemsRef.current = rendered;
    if (scrollerRef.current?.scrollHeight === settledHeightRef.current) {
      captureAnchor();
    }
  }

  useLayoutEffect(() => {
    const scroller = scrollerRef.current;
    const anchor = initialAnchorRef.current;
    if (!scroller) {
      return;
    }
    if (anchor.kind === "key" && scrollToKey(anchor.key, anchor.align)) {
      return;
    }
    scroller.scrollTop = scroller.scrollHeight;
    sync();
  }, [scrollToKey, sync]);

  useLayoutEffect(() => {
    settle();
  }, [rendered, settle]);

  useEffect(() => {
    const scroller = scrollerRef.current;
    const content = contentRef.current;
    if (!scroller || !content) {
      return;
    }
    const observer = new ResizeObserver(() => settle());
    observer.observe(scroller);
    observer.observe(content);
    return () => observer.disconnect();
  }, [settle]);

  return (
    <div
      ref={scrollerRef}
      data-message-scroller=""
      onScroll={handleScroll}
      onClickCapture={handleClickCapture}
      // Browser scroll anchoring would correct prepends a second time. Rubber
      // banding is off for the same reason: an iOS bounce still running when
      // older rows land pulls the corrected offset back to the top, which
      // loads the next page.
      style={{ overflowAnchor: "none", overscrollBehaviorY: "none" }}
      className={cn("overflow-y-auto outline-none", className)}
    >
      {/* Fewer rows than fill the viewport rest on the composer; the spare
      height sits above them and the header. */}
      <div ref={contentRef} className="flex min-h-full flex-col">
        <div aria-hidden className="flex-1" />
        {header}
        <div ref={rowsRef}>
          {rendered.map((item) => {
            const key = itemKey(item);
            return (
              <div key={key} data-scroller-key={key}>
                {renderItem(item)}
              </div>
            );
          })}
        </div>
        {footer}
      </div>
    </div>
  );
}
