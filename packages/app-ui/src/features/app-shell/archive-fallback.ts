/**
 * When the user archives the channel they're currently viewing, the
 * primary pane would otherwise point at a now-archived conversation. The
 * rule (locked with shiki):
 *
 *   1. **next active top-level** in sidebar order — the channel just below
 *      the archived one. This keeps the eye moving "down the list", the
 *      same direction the archived row vacated.
 *   2. else **previous active top-level** — when the archived channel was
 *      last in the list, fall back upward.
 *   3. else **null** — no active top-level channels remain; the caller
 *      clears selection to the workspace empty state.
 *
 * Only the PRIMARY selection follows this rule. Archiving from the
 * conversation pane, or restoring, must NOT steal primary selection — those
 * callers don't invoke this.
 *
 * Pure so the precedence is unit-tested independent of React/nav wiring;
 * the mutation callback only performs the nav side-effect with the result.
 *
 * `activeTopLevel` is the active top-level channel list in sidebar order,
 * as currently rendered (it still INCLUDES `archivedId` — the archive
 * mutation hasn't invalidated the list yet when this runs). Threads and
 * archived rows must be filtered out by the caller before passing in.
 */
export function pickFallbackAfterArchive(
  activeTopLevel: readonly { id: string }[],
  archivedId: string,
): string | null {
  const idx = activeTopLevel.findIndex((c) => c.id === archivedId);
  if (idx === -1) {
    // The archived channel isn't in the active top-level list (already
    // gone, or not top-level) — just pick the first remaining active one.
    return activeTopLevel.find((c) => c.id !== archivedId)?.id ?? null;
  }
  const next = activeTopLevel[idx + 1];
  if (next) return next.id;
  const prev = activeTopLevel[idx - 1];
  if (prev) return prev.id;
  return null;
}

/**
 * Default channel for the auto-select effect, EXCLUDING a recently-archived
 * primary id.
 *
 * After archiving the channel in the primary pane with no neighbor to fall
 * back to, App clears selection to the workspace empty state. But the
 * auto-select effect then runs with `selectedChannelId === null` BEFORE the
 * React Query invalidation has refetched the active channel list — so the
 * stale `activeTopLevel` it sees can still contain the just-archived row.
 * Without excluding it, the effect's "pick the first active top-level"
 * fallback would immediately re-open the channel the user just archived,
 * undoing the clear. Deleting the remembered-channel entry only guards the
 * *remembered* branch; this guards the *fallback* branch.
 *
 * `excludeId` is null in the normal case (no pending archive), so this is
 * just "first active top-level, else null".
 */
export function pickDefaultChannel(
  activeTopLevel: readonly { id: string }[],
  excludeId: string | null,
): string | null {
  return activeTopLevel.find((c) => c.id !== excludeId)?.id ?? null;
}
