/**
 * Shared pane paint. Pair with data-panel for geometry and borders, whose
 * single authority is the panel rule in surfaces.css. Themes override its tokens
 * and selectors without competing with inline border-radius declarations.
 *
 * Layout/overflow stays with each caller.
 */
export const PANE_SHADOW = "var(--pane-shadow, var(--card-shadow, none))";
export const PANE_BG = "var(--pane-bg, var(--bg-base))";

export const paneChrome = {
  background: PANE_BG,
  boxShadow: PANE_SHADOW,
} as const;

/** Reuse an existing role unless a surface needs distinct theme tokens. */
export type PanelRole = "sidebar" | "chat" | "detail" | "side";
