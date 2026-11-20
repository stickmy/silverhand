import { forwardRef, type PointerEvent as ReactPointerEvent } from "react";

interface SplitterTrackProps {
  onPointerDown?: (e: ReactPointerEvent) => void;
  onPointerMove?: (e: ReactPointerEvent) => void;
  onPointerUp?: (e: ReactPointerEvent) => void;
  onPointerCancel?: (e: ReactPointerEvent) => void;
  /**
   * Active-drag paint (`data-[active=true]:` below). Omit it to toggle
   * `data-active` imperatively on the forwarded ref instead, so a rAF drag
   * never re-renders mid-drag.
   */
  active?: boolean;
}

/**
 * 4px-wide draggable column divider — the shared paint + geometry for every
 * resizable seam (sidebar↔conversation and activity-list↔channel via
 * `VerticalSplitter`; between stage panes in `ConversationStage`). Owns the
 * track's LOOK and PLACEMENT only; each parent wires its own drag mechanism and
 * passes pointer handlers (+ optionally `active` / a ref) through.
 *
 * PAINT. Transparent by default (`--splitter-bg`, neutral fallback
 * `--border-subtle`); hover or active drag tints with `--splitter-hover-bg` — a
 * neutral ink every theme sets (fence-enforced in appearance.test.ts, since
 * poster declares it). The `var(--border)` fallback is a neutral safety net: a
 * divider is a utility signal, so it must never fall back to `--accent` — a
 * saturated accent on a 4px separator reads as "look at this!" and steals the
 * accent budget (v6.9.6 #task:917: poster's fallback used to map to hot brand
 * pink).
 *
 * GEOMETRY (#task:1965 review round 1). The track is a flex item, so without
 * help it would add its own 4px column ON TOP of the sheet's `--pane-gap`,
 * pushing the panes `gap + 4 + gap` apart. The negative inline margin pulls it
 * back so the VISIBLE distance between panes stays exactly `--pane-gap` with the
 * track centred in it — derived from the gap here (not a per-theme
 * `--splitter-offset` token), so a theme that changes its gap and forgets the
 * offset can't get a silently wrong seam (exactly what happened when iris took
 * the gap to 0). At `--pane-gap: 0` the track straddles the seam (±2px into each
 * pane), which is why it needs `zIndex`: both panes are positioned, so a later
 * sibling would otherwise paint its background over the track's right half. For
 * gapped themes the track sits inside the empty gutter, so the stacking order is
 * a no-op there.
 */
export const SplitterTrack = forwardRef<HTMLDivElement, SplitterTrackProps>(function SplitterTrack(
  { onPointerDown, onPointerMove, onPointerUp, onPointerCancel, active },
  ref,
): JSX.Element {
  return (
    <div
      ref={ref}
      role="separator"
      aria-orientation="vertical"
      data-active={active ? "true" : undefined}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={onPointerCancel}
      style={{
        marginInline: "calc((var(--pane-gap) + 4px) / -2)",
        zIndex: 1,
      }}
      // A touch drag would otherwise scroll the page, or a held press select
      // the track as text instead of resizing.
      className={
        "relative w-1 shrink-0 cursor-col-resize touch-none select-none [-webkit-touch-callout:none] bg-(--splitter-bg,var(--border-subtle)) " +
        "transition-colors hover:bg-(--splitter-hover-bg,var(--border)) " +
        "data-[active=true]:bg-(--splitter-hover-bg,var(--border))"
      }
    >
      {/* Wider invisible hit-target so users don't need pixel-perfect aim. */}
      <div className="absolute -inset-x-1.5 inset-y-0" />
    </div>
  );
});
