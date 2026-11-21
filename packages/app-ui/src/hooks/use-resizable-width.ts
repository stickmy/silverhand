import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";

interface Options {
  /** localStorage key the width is persisted under. */
  storageKey: string;
  /** Width used on first render (and when storage holds a bad value). */
  defaultWidth: number;
  min: number;
  max: number;
  /** Optional CSS custom property (e.g. `--sidebar-width`) to mirror
   *  the width into. Set on `document.documentElement` so existing
   *  `var(--name)` styles in unrelated components stay reactive. */
  cssVar?: string;
  /** The edge the splitter sits on; a left-edge splitter widens leftward. */
  edge?: "left" | "right";
  /** Read when a drag starts: the width actually laid out and the most the
   *  column can take there. A flex row may have shrunk it below its stored
   *  width, and growing past what the neighbours can give would push it the
   *  other way. */
  bounds?: () => { width: number; max: number } | null;
}

export interface ResizableWidth {
  width: number;
  /** Attach to a 4px splitter element on the column's right edge. */
  startResize: (e: React.PointerEvent) => void;
  isDragging: boolean;
  /** Adopt a width for this session without persisting it. */
  pin: (next: number) => void;
}

/**
 * Persistent column width with a draggable splitter on the right edge.
 * The hook owns the localStorage round-trip and (optionally) mirrors
 * the width into a CSS custom property so existing `w-[var(--…)]`
 * call sites pick up the change without re-plumbing.
 *
 * Drag handling listens on `document` (rather than the splitter
 * element) so the gesture survives the cursor leaving the 4px hit
 * region — important for thin handles.
 */
export function useResizableWidth({
  storageKey,
  defaultWidth,
  min,
  max,
  cssVar,
  edge = "right",
  bounds,
}: Options): ResizableWidth {
  const readStored = (): number => {
    try {
      const raw = localStorage.getItem(storageKey);
      const n = raw !== null ? Number(raw) : NaN;
      if (Number.isFinite(n) && n >= min && n <= max) return n;
    } catch {
      /* private mode — fall through to default */
    }
    return defaultWidth;
  };
  const [width, setWidth] = useState<number>(readStored);
  // A caller may switch keys (one width per sidebar style); the new key's
  // stored width replaces the old one in the same render.
  const [loadedKey, setLoadedKey] = useState(storageKey);
  if (loadedKey !== storageKey) {
    setLoadedKey(storageKey);
    setWidth(readStored());
  }

  const widthRef = useRef(width);
  const frameRef = useRef<number | null>(null);

  const persistWidth = useCallback(
    (next: number): void => {
      try {
        localStorage.setItem(storageKey, String(next));
      } catch {
        /* quota or private mode — in-memory is fine */
      }
    },
    [storageKey],
  );

  useLayoutEffect(() => {
    widthRef.current = width;
    if (!cssVar) return;
    document.documentElement.style.setProperty(cssVar, `${width}px`);
  }, [width, cssVar]);

  useEffect(() => {
    return () => {
      if (frameRef.current !== null) cancelAnimationFrame(frameRef.current);
    };
  }, []);

  const scheduleCssWidth = useCallback(
    (next: number): void => {
      widthRef.current = next;

      if (!cssVar) {
        setWidth(next);
        return;
      }

      if (frameRef.current !== null) return;
      frameRef.current = requestAnimationFrame(() => {
        frameRef.current = null;
        document.documentElement.style.setProperty(cssVar, `${widthRef.current}px`);
      });
    },
    [cssVar],
  );

  const flushCssWidth = useCallback((): void => {
    if (!cssVar) return;
    if (frameRef.current !== null) {
      cancelAnimationFrame(frameRef.current);
      frameRef.current = null;
    }
    document.documentElement.style.setProperty(cssVar, `${widthRef.current}px`);
  }, [cssVar]);

  const dragOriginX = useRef(0);
  const dragOriginWidth = useRef(0);
  const [dragging, setDragging] = useState(false);

  const startResize = useCallback(
    (e: React.PointerEvent) => {
      e.preventDefault();
      const laid = bounds?.() ?? null;
      const ceiling = laid ? Math.max(min, Math.min(max, laid.max)) : max;
      dragOriginX.current = e.clientX;
      dragOriginWidth.current = laid ? Math.min(widthRef.current, laid.width) : widthRef.current;
      // A column the row has already squeezed below `min` must not jump wider
      // on the first move.
      const floor = Math.min(min, dragOriginWidth.current);
      setDragging(true);

      const onMove = (ev: PointerEvent): void => {
        const dx = (ev.clientX - dragOriginX.current) * (edge === "left" ? -1 : 1);
        const next = Math.max(floor, Math.min(ceiling, dragOriginWidth.current + dx));
        scheduleCssWidth(next);
      };
      const onUp = (): void => {
        flushCssWidth();
        const next = widthRef.current;
        setWidth(next);
        persistWidth(next);
        setDragging(false);
        document.removeEventListener("pointermove", onMove);
        document.removeEventListener("pointerup", onUp);
        document.removeEventListener("pointercancel", onUp);
        document.body.style.cursor = "";
        document.body.style.userSelect = "";
      };

      document.addEventListener("pointermove", onMove);
      document.addEventListener("pointerup", onUp);
      document.addEventListener("pointercancel", onUp);
      document.body.style.cursor = "col-resize";
      document.body.style.userSelect = "none";
    },
    [bounds, edge, flushCssWidth, max, min, persistWidth, scheduleCssWidth],
  );

  const pin = useCallback((next: number): void => {
    widthRef.current = next;
    setWidth(next);
  }, []);

  return { width, startResize, isDragging: dragging, pin };
}
