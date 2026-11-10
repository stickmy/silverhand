// Viewer-scoped sorting. Sidebar is a server-ordered, manual-only flat list;
// channels and favorites retain their existing recent/manual behavior.
// See docs/sidebar-sort.md.
import { z } from 'zod';

/**
 * Per-list sort mode. One [recent|manual] toggle per list; absence of a
 * stored row means the default, 'recent' (recently-OPENED by this viewer).
 */
export const SortModeSchema = z.enum(['recent', 'manual']);
export type SortMode = z.infer<typeof SortModeSchema>;

/**
 * Hard-group descriptor for a manual reorder. Groups are partitioned LIVE at
 * sort time, never stored on the entity. Exactly ONE list has star at all
 * (#task:1899) and there it is EXCLUSIVE (#task:1888): the CONVERSATION list.
 * Star never creates a hard group anywhere.
 *   channel sidebar: 'channels' — a single group holding the workspace's
 *     conversations. A starred conversation has MOVED to the global
 *     Favorites/STARRED list and is reordered there, so it is excluded from this
 *     group's reorder (it stays a valid star/touch target and keeps `starred_at`
 *     on the wire). One list-wide mode.
 *   favorites list : 'favorites' — a single FLAT group (#task:1252 C). The global
 *     Favorites list is sortable on its own axis (recent/manual + drag) but is not
 *     sub-partitioned: every member sits in the one `favorites` group.
 * Star is per-viewer personal state — it lives in the viewer's sort entries
 * (`starred_at`), never as a shared aggregate field.
 */
export const SortGroupSchema = z.enum([
  'channels', // one workspace's conversations (#task:1888) — star moved them out
  'favorites',
  'sidebar',
]);
export type SortGroup = z.infer<typeof SortGroupSchema>;

/**
 * `list_key` namespaces a list:
 *   'sidebar'           — server-ordered top-level branches, manual-only
 *   'channels:<ws_id>'  — one workspace's conversation list
 *   'favorites'         — the global Favorites list's ordering state (#task:1252 C):
 *                         rank + the list-wide mode only, keyed by
 *                         `conversation:<entity_id>` favorite keys. Membership
 *                         stays single-source in each
 *                         entity's HOME list (`starred_at`); `favorites` never
 *                         stores what is starred, only how the list is arranged.
 *                         Since #task:1888 its members are CONVERSATIONS, so the
 *                         home list of every favorite is a `channels:<ws_id>` one.
 */
export const SortListKeySchema = z
  .string()
  .refine((v) => v === 'sidebar' || v === 'favorites' || v.startsWith('channels:'), {
    message: "list_key must be 'sidebar', 'favorites', or 'channels:<ws_id>'",
  });

/** Per-entity viewer state. rank/last_accessed_at/starred_at are null until set.
 *  `starred_at` is the personal ⭐ favorite (#task:1209): non-null = starred. Only
 *  a CONVERSATION can carry one (#task:1899), and there it is the home-EXCLUSION
 *  signal (#task:1888) — the conversation has MOVED to the global Favorites list,
 *  so its parent `channels:*` list hides it by reading this off its OWN inline
 *  `viewer_sort` (never by joining `GET /api/favorites`). It is a PRESENCE marker (+ timestamp for
 *  the favorites projection / debug), NOT a within-group sort key — within-group
 *  order is governed by that group's mode (recent → last_accessed_at / manual →
 *  rank). It REPLACES the former shared `pinned_at` aggregate field — star is
 *  per-viewer view state, so it lives here in the sort entry, never on the entity
 *  DTO. */
export const ViewerSortEntryDtoSchema = z.object({
  entity_id: z.string(),
  rank: z.number().nullable(),
  last_accessed_at: z.number().nullable(),
  starred_at: z.number().nullable(),
});
export type ViewerSortEntryDto = z.infer<typeof ViewerSortEntryDtoSchema>;

/**
 * The viewer's sort state for one list. Carried inline on list responses
 * (channel list, favorites list) and returned authoritatively by the
 * mode/order mutations so the renderer can reconcile an optimistic drag.
 *
 * `modes` is the per-SCOPE mode map (#task:1146). A scope is one independently
 * toggleable [recent|manual] axis within the list:
 *   - channel sidebar: scope = '' (the sentinel "list-wide" scope) — one mode
 *     for the whole list, exactly as before.
 * Sparse: an absent scope means the default, 'recent'. Read with the renderer
 * helper `sortModeOf(state, group?)` (group omitted ⇒ scope '').
 */
export const ViewerSortStateDtoSchema = z.object({
  list_key: z.string(),
  modes: z.record(SortModeSchema),
  entries: z.array(ViewerSortEntryDtoSchema),
});
export type ViewerSortStateDto = z.infer<typeof ViewerSortStateDtoSchema>;

/**
 * Set one scope's mode. `group` selects the scope:
 *   - list_key 'channels:<id>' → `group` MUST be omitted (list-wide, scope '').
 * The route enforces this and rejects a mismatch (400) rather than guessing.
 */
export const SetSortModeInputSchema = z.object({
  list_key: SortListKeySchema,
  group: SortGroupSchema.optional(),
  mode: SortModeSchema,
});
export type SetSortModeInput = z.infer<typeof SetSortModeInputSchema>;

/**
 * Whole-list-within-group reorder. `ordered_ids` is the new order of ONE
 * group's entities (renderer only sends within-group ids). The backend
 * validates every id lives in `group` (live star/ownership) and rewrites their
 * dense ranks — leaving other groups untouched (grouping is sort-time).
 */
export const SortReorderInputSchema = z.object({
  list_key: SortListKeySchema,
  group: SortGroupSchema,
  ordered_ids: z.array(z.string()),
});
export type SortReorderInput = z.infer<typeof SortReorderInputSchema>;

/**
 * Stamp recent-opened. Called ONLY on selection (open a channel);
 * never on message arrival, unread change, or archive restore. Idempotent
 * upsert of last_accessed_at.
 */
export const SortTouchInputSchema = z.object({
  list_key: SortListKeySchema,
  entity_id: z.string(),
});
export type SortTouchInput = z.infer<typeof SortTouchInputSchema>;

/** Stable top-level identities; labels, counts and visibility do not change them. */
export const SIDEBAR_SORT_LIST_KEY = 'sidebar' as const;
export const SIDEBAR_SORT_GROUP = 'sidebar' as const;
export const SIDEBAR_FIXED_BRANCH_IDS = ['section:starred', 'section:direct_messages'] as const;
export function sidebarWorkspaceBranchId(workspaceId: string): string {
  return `workspace:${workspaceId}`;
}

/** Server-ordered complete branch set, including hidden/empty fixed branches.
 * Default: Starred, Direct messages, regular workspaces, agent spaces; workspace
 * ties use created_at then id. Ranked branches precede new/unranked branches.
 * Consume entries in response order, never through a recent-mode sorter.
 * PUT /api/sort/order submits ALL entity_ids with list_key/group = sidebar.
 * 409 means refetch this read and reconcile the drag against its complete set.
 * Sidebar touch/mode are rejected. Unsupported list keys are rejected by the schema.
 */
export const SidebarSortResponseSchema = z.object({
  viewer_sort: ViewerSortStateDtoSchema.extend({
    list_key: z.literal(SIDEBAR_SORT_LIST_KEY),
    modes: z.object({ '': z.literal('manual') }),
  }),
});
export type SidebarSortResponse = z.infer<typeof SidebarSortResponseSchema>;

/** Viewer-scoped WS invalidation after a committed sidebar reorder.
 * Payload carries no order or entity IDs; refetch the authorized sidebar read.
 * Also refetch after viewer reconnect and workspace visibility changes.
 */
export const SIDEBAR_SORT_CHANGED_EVENT = 'sidebar_sort_changed' as const;
