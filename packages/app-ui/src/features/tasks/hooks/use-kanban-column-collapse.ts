import { useSyncExternalStore } from "react";
import type { TaskStatus } from "@silverhand/shared";

/**
 * Per-column collapse state for the task kanban (global NavRail section +
 * per-conversation Tasks tab share one store, so toggling in one surface
 * reflects in the other within the session).
 *
 * Persistence is deliberately **in-memory only** (#task:1169, Alice + Cindy
 * review): column collapse is a per-viewer UI preference of the same family as
 * list-sort mode/rank, whose persistence model is already fixed to
 * server-side viewer-scope in `docs/list-sort-v1.md` ("not localStorage —
 * Silverhand is multi-surface; localStorage would split per surface").
 * Reaching for localStorage here would (a) fork that state per
 * desktop/web/standalone surface and (b) open a second persistence paradigm
 * that drifts from list-sort. So we keep zero cross-session persistence: a
 * user's toggles live in a module-level store and are shared across the global
 * section, the per-conversation Tasks tab, and navigation **within the
 * session**, but a window
 * reload / fresh session resets to the default (Done and Closed collapsed).
 *
 * The public interface (`isCollapsed` / `toggle`) is the contract: if
 * "remember my expanded columns" ever becomes a real requirement, swap the
 * module store below for the server-side viewer-scoped read model — the call
 * sites in TasksSection / ChannelTasksTab don't change.
 */

// The terminal groups start collapsed: they hold the bulk of historical tasks,
// so an open Done would make the stacked board very long and cost the first
// mount.
const DEFAULT_COLLAPSED: readonly TaskStatus[] = ["done", "closed"];

// Module-level single source of truth. Snapshot identity only changes when a
// column is actually toggled, so `useSyncExternalStore` stays stable.
let collapsed: ReadonlySet<TaskStatus> = new Set(DEFAULT_COLLAPSED);
const listeners = new Set<() => void>();

function subscribe(onChange: () => void): () => void {
  listeners.add(onChange);
  return () => {
    listeners.delete(onChange);
  };
}

function getSnapshot(): ReadonlySet<TaskStatus> {
  return collapsed;
}

/** Toggle a column's collapsed state and notify every mounted surface. */
export function toggleColumnCollapse(status: TaskStatus): void {
  const next = new Set(collapsed);
  if (next.has(status)) next.delete(status);
  else next.add(status);
  collapsed = next;
  for (const notify of listeners) notify();
}

/**
 * Force one column open — idempotent, and a no-op (no notify, no new snapshot)
 * when it is already open.
 *
 * Reveal needs this, not `toggle`: Done and Closed are collapsed by default, so
 * revealing a finished task renders no row for it and the scroll+pulse lands on nothing —
 * the affordance silently does nothing. `toggle` cannot express "make sure it is
 * open" (it would close an already-open column), and the no-op arm matters
 * because every mounted board re-renders on notify.
 */
export function expandColumn(status: TaskStatus): void {
  if (!collapsed.has(status)) return;
  const next = new Set(collapsed);
  next.delete(status);
  collapsed = next;
  for (const notify of listeners) notify();
}

export interface KanbanColumnCollapse {
  isCollapsed: (status: TaskStatus) => boolean;
  toggle: (status: TaskStatus) => void;
}

export function useKanbanColumnCollapse(): KanbanColumnCollapse {
  const set = useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
  return {
    isCollapsed: (status: TaskStatus) => set.has(status),
    toggle: toggleColumnCollapse,
  };
}
