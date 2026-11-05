import { createDrawnIcon } from "./icon";

// Glyphs drawn for the product instead of taken from Lucide, as plain SVG on
// the shared 24px grid and 2 round stroke. Outlines are native `<rect rx>` and
// `<circle>`, opened with clip paths: Chrome's GPU rasteriser antialiases
// curved `<path>` strokes coarsely, while native shapes stay smooth. Where a
// clip cuts a stroke, a zero-length `h0` path restores the round cap.

/** A lidded box with a handle slot. */
export const ArchiveIcon = createDrawnIcon("archive", ({ id }) => (
  <>
    <clipPath id={`${id}body`}>
      <path d="M0 8H24V24H0Z" />
    </clipPath>
    <rect x="2.25" y="3" width="19.5" height="4.5" rx="1.5" />
    <rect x="4" y="0" width="16" height="21" rx="3.5" clipPath={`url(#${id}body)`} />
    <rect x="9" y="10.5" width="6" height="3" rx="1.5" />
  </>
));

/** The archive box with an arrow out of it. */
export const UnarchiveIcon = createDrawnIcon("unarchive", ({ id }) => (
  <>
    <clipPath id={`${id}body`}>
      <path d="M0 8H24V24H0Z" />
    </clipPath>
    <rect x="2.25" y="3" width="19.5" height="4.5" rx="1.5" />
    <rect x="4" y="0" width="16" height="21" rx="3.5" clipPath={`url(#${id}body)`} />
    <path d="M12 17.5v-6.75m-2.5 2.5 2.5-2.5 2.5 2.5" />
  </>
));

/** A three-quarter ring; spun by the consumer (`animate-spin`). */
export const SpinnerIcon = createDrawnIcon("spinner", ({ id }) => (
  <>
    <clipPath id={`${id}gap`}>
      <path d="M-1 -1H25V25H-1ZM12 12L12 -1L25 -1L25 12Z" clipRule="evenodd" />
    </clipPath>
    <circle cx="12" cy="12" r="9" clipPath={`url(#${id}gap)`} />
    <path d="M12 3h0M21 12h0" />
  </>
));

/** Task in progress: a ring with a three-quarter pie, between the empty ring and the check. */
export const ProgressIcon = createDrawnIcon("progress", () => (
  <>
    <circle cx="12" cy="12" r="10" />
    <path d="M12 12V6a6 6 0 1 1-6 6Z" fill="currentColor" stroke="none" />
  </>
));

const Bubble = ({ id }: { id: string }) => (
  <>
    <clipPath id={`${id}tail`}>
      <path d="M-1 -1H25V25H-1ZM12 12L6.82 31.32L-3.32 24.86Z" clipRule="evenodd" />
    </clipPath>
    <circle cx="12" cy="12" r="9" clipPath={`url(#${id}tail)`} />
    <path d="M5.11 17.79L3.5 20.5L9.67 20.69" />
  </>
);

/** A message: one round bubble. */
export const MessageIcon = createDrawnIcon("message", Bubble);

export const CopyIcon = createDrawnIcon("copy", ({ id }) => (
  <>
    <clipPath id={`${id}back`}>
      <path d="M0 0H24V8H8V24H0Z" />
    </clipPath>
    <rect x="8" y="8" width="14" height="14" rx="4" />
    <rect x="2" y="2" width="14" height="14" rx="4" clipPath={`url(#${id}back)`} />
  </>
));

export const DeleteIcon = createDrawnIcon("delete", ({ id }) => (
  <>
    <clipPath id={`${id}body`}>
      <path d="M0 6H24V24H0Z" />
    </clipPath>
    <clipPath id={`${id}lid`}>
      <path d="M0 0H24V6H0Z" />
    </clipPath>
    <path d="M3 6h18" />
    <rect x="5" y="1" width="14" height="21" rx="3.5" clipPath={`url(#${id}body)`} />
    <rect x="8" y="2" width="8" height="8" rx="2.5" clipPath={`url(#${id}lid)`} />
    <path d="M10 11v6M14 11v6" />
  </>
));

export const LockIcon = createDrawnIcon("lock", ({ id }) => (
  <>
    <clipPath id={`${id}shackle`}>
      <path d="M0 0H24V7H0Z" />
    </clipPath>
    <rect x="3" y="11" width="18" height="11" rx="3.5" />
    <circle cx="12" cy="7" r="5" clipPath={`url(#${id}shackle)`} />
    <path d="M7 7v4M17 7v4" />
  </>
));

export const UnlockIcon = createDrawnIcon("unlock", ({ id }) => (
  <>
    <clipPath id={`${id}shackle`}>
      <path d="M0 0H24V6H12V7H0Z" />
    </clipPath>
    <rect x="3" y="11" width="18" height="11" rx="3.5" />
    <circle cx="12" cy="7" r="5" clipPath={`url(#${id}shackle)`} />
    <path d="M7 7v4M16.9 6h0" />
  </>
));

export const ComputerIcon = createDrawnIcon("computer", () => (
  <>
    <rect x="2" y="3" width="20" height="14" rx="3.5" />
    <path d="M8 21h8M12 17v4" />
  </>
));

export const SidebarCloseIcon = createDrawnIcon("sidebar-close", () => (
  <>
    <rect x="3" y="3" width="18" height="18" rx="4" />
    <path d="M9 3v18" />
    <path d="m16 15-3-3 3-3" />
  </>
));

export const SidebarOpenIcon = createDrawnIcon("sidebar-open", () => (
  <>
    <rect x="3" y="3" width="18" height="18" rx="4" />
    <path d="M9 3v18" />
    <path d="m14 9 3 3-3 3" />
  </>
));

export const LeaveIcon = createDrawnIcon("leave", ({ id }) => (
  <>
    <clipPath id={`${id}door`}>
      <path d="M0 0H9V24H0Z" />
    </clipPath>
    <rect x="3" y="3" width="14" height="18" rx="3.5" clipPath={`url(#${id}door)`} />
    <path d="M9 3h0M9 21h0" />
    <path d="M21 12H9" />
    <path d="m16 17 5-5-5-5" />
  </>
));

export const JoinIcon = createDrawnIcon("join", ({ id }) => (
  <>
    <clipPath id={`${id}door`}>
      <path d="M15 0H24V24H15Z" />
    </clipPath>
    <rect x="7" y="3" width="14" height="18" rx="3.5" clipPath={`url(#${id}door)`} />
    <path d="M15 3h0M15 21h0" />
    <path d="M15 12H3" />
    <path d="m10 17 5-5-5-5" />
  </>
));

const Page = ({ id }: { id: string }) => (
  <>
    <clipPath id={`${id}page`}>
      <path d="M0 0H14V8H24V24H0Z" />
    </clipPath>
    <rect x="4" y="2" width="16" height="20" rx="3.5" clipPath={`url(#${id}page)`} />
    <path d="M14 2l6 6" />
    <path d="M14 2v4.5A1.5 1.5 0 0 0 15.5 8H20" />
  </>
);

export const FileIcon = createDrawnIcon("file", Page);

export const DocumentIcon = createDrawnIcon("document", ({ id }) => (
  <>
    <Page id={id} />
    <path d="M8 13h8M8 17h8M8 9h2" />
  </>
));

export const ImageIcon = createDrawnIcon("image", () => (
  <>
    <rect x="3" y="3" width="18" height="18" rx="4" />
    <circle cx="9" cy="9" r="2" />
    <path d="m21 15-3.086-3.086a2 2 0 0 0-2.828 0L6 21" />
  </>
));

// Clockwise around the ring; the centre cell stays still as the hub.
const GRID_RING = [
  [0, 0],
  [1, 0],
  [2, 0],
  [2, 1],
  [2, 2],
  [1, 2],
  [0, 2],
  [0, 1],
] as const;

/** A 3×3 grid of squares; a highlight runs clockwise around the outer ring
 *  (`.grid-loader` in `surfaces.css`). */
export const GridLoaderIcon = createDrawnIcon("grid-loader", () => (
  <g stroke="none" fill="currentColor">
    <rect className="grid-loader-hub" x="9.5" y="9.5" width="5" height="5" rx="1" />
    {GRID_RING.map(([col, row], index) => (
      <rect
        key={index}
        className="grid-loader-cell"
        style={{ animationDelay: `${index * 100 - 800}ms` }}
        x={2 + col * 7.5}
        y={2 + row * 7.5}
        width="5"
        height="5"
        rx="1"
      />
    ))}
  </g>
));
