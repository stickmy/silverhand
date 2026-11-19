import * as HoverCard from "@radix-ui/react-hover-card";
import { useEffect, useRef, useState, type ReactNode } from "react";

/**
 * Hover-triggered popover used by inline reference chips.
 *
 * Opens after a 500ms grace so brief flick-throughs (mouse moving past
 * a paragraph) don't paint a popover at all. After the grace, the
 * caller decides whether to render real content or a skeleton — the
 * caller already owns the data fetch via React Query, so this
 * component is intentionally dumb.
 *
 * Visible-state contract (set by the user, see `mtRX9X1H93dDVDKw`):
 *   - 0–500ms hover  → popover NOT visible. Fetch races in the
 *                       background (callers mount their queries
 *                       eagerly), so by the time the popover opens
 *                       the cache is usually warm.
 *   - ≥ 500ms, data ready → render content directly.
 *   - ≥ 500ms, still loading → render skeleton (NO spinner — that
 *                              was explicitly ruled out in design).
 *
 * `closeDelay={120}` gives the user a brief window to move from chip
 * → popover so the popover doesn't snap shut between them. Defaults
 * here mirror radix recommendations for hover-card "open delay" UX.
 */
export interface HoverPopoverProps {
  trigger: ReactNode;
  /** Real popover body (caller resolved data). */
  content: ReactNode;
  /** Optional skeleton — shown when content is null/undefined and
   *  the popover has already opened past the 500ms grace. Pass a
   *  caller-styled placeholder; we don't impose a default look so
   *  task vs conversation popovers can size themselves. */
  skeleton?: ReactNode;
  /** Whether the data the caller passes via `content` is ready. When
   *  false, the popover shows `skeleton` instead. */
  isContentReady: boolean;
  /** Optional override of the standard 500ms grace. Only the test
   *  harness uses this — production callers should leave it default. */
  openDelayMs?: number;
  /** When true, the popover stays open regardless of hover state. Used
   *  when the content hosts a nested interactive layer (e.g. the task
   *  popover's status picker, which portals outside the hover-card DOM
   *  — moving the pointer onto it would otherwise trip the close delay
   *  and unmount the picker mid-interaction). The caller flips this on
   *  while that nested layer is open. */
  keepOpen?: boolean;
  /** When true, the card is closed and stays closed until the pointer next
   *  enters the trigger; a control inside the trigger that opens its own
   *  layer sets this, so the two never show together. */
  suppressed?: boolean;
}

export function HoverPopover({
  trigger,
  content,
  skeleton,
  isContentReady,
  openDelayMs = 500,
  keepOpen = false,
  suppressed = false,
}: HoverPopoverProps): JSX.Element {
  // Controlled so `keepOpen` can pin the card open while a nested
  // interactive layer (status picker) is active. Hover/focus still
  // drive `hoverOpen` via Radix's own detection; we OR in `keepOpen`.
  const [hoverOpen, setHoverOpen] = useState(false);
  // When the pinned layer closes (keepOpen true→false), force the card
  // shut instead of trusting `hoverOpen`. The nested picker portals
  // outside the hover-card DOM and swallows the pointer-leave / blur
  // events Radix relies on, so `hoverOpen` can be stuck `true` with no
  // way left to dismiss (shiki: 点击 status 后卡片无法失焦消失). Collapsing
  // on pin-release is the deterministic exit — after the user finishes
  // with the picker, the popover just goes away.
  const prevKeepOpen = useRef(keepOpen);
  useEffect(() => {
    if (prevKeepOpen.current && !keepOpen) setHoverOpen(false);
    prevKeepOpen.current = keepOpen;
  }, [keepOpen]);
  useEffect(() => {
    if (suppressed) setHoverOpen(false);
  }, [suppressed]);
  return (
    <HoverCard.Root
      open={!suppressed && (hoverOpen || keepOpen)}
      onOpenChange={(next) => {
        if (next && suppressed) return;
        setHoverOpen(next);
      }}
      openDelay={openDelayMs}
      closeDelay={120}
    >
      <HoverCard.Trigger asChild>{trigger}</HoverCard.Trigger>
      <HoverCard.Portal>
        <HoverCard.Content
          align="start"
          sideOffset={6}
          collisionPadding={8}
          data-popover-frame="info"
          className="z-(--z-popover) w-65 max-w-[calc(100vw-32px)] bg-bg-floating p-3 text-xs text-text-primary shadow-popover outline-none data-[state=open]:animate-in data-[state=open]:fade-in-0 data-[state=open]:zoom-in-95"
        >
          {isContentReady ? content : (skeleton ?? <DefaultSkeleton />)}
        </HoverCard.Content>
      </HoverCard.Portal>
    </HoverCard.Root>
  );
}

/**
 * Three muted-bar shimmer that any caller can use when they don't
 * want to roll their own skeleton. Width pattern (full / 60% / 40%)
 * suggests a title + meta line layout without committing to one.
 */
function DefaultSkeleton(): JSX.Element {
  return (
    <div className="space-y-1.5" aria-hidden>
      <div className="h-3 w-full animate-pulse rounded bg-bg-elevated" />
      <div className="h-3 w-3/5 animate-pulse rounded bg-bg-elevated" />
      <div className="h-3 w-2/5 animate-pulse rounded bg-bg-elevated" />
    </div>
  );
}
