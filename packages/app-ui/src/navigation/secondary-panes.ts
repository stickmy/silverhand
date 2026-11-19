/**
 * Registry of the right-pane "secondary family" — the single source for every
 * pane-keyed list: the opener's clear-list, the stacking order (render surface,
 * ⌘W, compact back), and which query params make a page a detail page. Key
 * order IS the stacking order, top first, so adding a pane is one entry here.
 *
 * `mode` is an intrinsic property of the pane KIND, never a caller argument —
 * so the illegal "open `side` as an overlay" combination has no API surface at
 * all (it's unconstructable, not merely gated).
 *
 *  - `mode: 'replace'` — conversation-context panes (`side`/`thread`/`threads`).
 *    Opening one writes itself and clears EVERY other secondary param (+
 *    `fullscreen`): one conversation context at a time, no restore stack.
 *  - `mode: 'overlay'` — utility/inspector panes (`agent`/`files`/`task`).
 *    Opening one writes itself, drops `fullscreen`, clears `?side` and the
 *    OTHER overlays, but PRESERVES every pane flagged `survivesOverlay` as the
 *    URL restore stack — closing brings the covered pane back with no in-memory
 *    state.
 *
 * `survivesOverlay` is a second axis because `mode` alone underdetermines the
 * restore stack: `side` is `replace` yet must NOT survive being covered. The
 * two `false` values are NOT the same kind of `false` — do not "unify" them:
 *  - `side: false` is a HARD CORRECTNESS invariant, never flippable to `true`.
 *    A hidden `?side` still feeds `useSecondaryPaneModel`'s visible/focused set
 *    (driving notification suppression + read-advance), so leaving it set while
 *    invisible is data corruption. The runtime matrix test locks this — it is a
 *    gate, not a style choice; do not remove it.
 *  - `agent`/`files`/`task` `false` is a SOFT TOPOLOGY choice — "one overlay on
 *    top at a time." These panes are visually neutral (no visible/focused
 *    side-effect), so this could flip to `true` later (e.g. if an agent panel
 *    is allowed to coexist with another overlay) without hurting correctness.
 *  - `thread`/`threads` `true`: pure render panes with no visibility
 *    side-effect, so an overlay preserves them as the restore stack.
 */
export const SECONDARY_PANES = {
  // `overlay` utility/inspector panes. survivesOverlay:false here is a SOFT
  // topology choice ("one overlay at a time"), flippable without correctness risk.
  agent: { mode: "overlay", survivesOverlay: false },
  human: { mode: "overlay", survivesOverlay: false },
  settings: { mode: "overlay", survivesOverlay: false },
  // A preview opened from the Attachments list keeps `files` beneath it, so it
  // stacks above files and peels first.
  attachment: { mode: "overlay", survivesOverlay: false },
  files: { mode: "overlay", survivesOverlay: false },
  task: { mode: "overlay", survivesOverlay: false },
  // `replace` conversation-context panes.
  //   side: covered-but-in-URL is data corruption → false is a HARD invariant.
  side: { mode: "replace", survivesOverlay: false },
  //   thread/threads: pure render, safe to keep under an overlay (restore stack).
  thread: { mode: "replace", survivesOverlay: true },
  threads: { mode: "replace", survivesOverlay: true },
} as const satisfies Record<string, { mode: "overlay" | "replace"; survivesOverlay: boolean }>;

/** Every secondary-pane query param, derived from the registry. */
export type SecondaryPane = keyof typeof SECONDARY_PANES;

/** Top-first stacking order. */
export const SECONDARY_PANE_ORDER = Object.keys(SECONDARY_PANES) as SecondaryPane[];

/** The pane drawn on top of the open ones, or `null` when none is open. */
export function topSecondaryPane(
  open: Readonly<Partial<Record<SecondaryPane, boolean>>>,
): SecondaryPane | null {
  return SECONDARY_PANE_ORDER.find((pane) => open[pane]) ?? null;
}
