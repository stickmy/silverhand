/**
 * Per-conversation view tabs.
 *
 * A conversation's main pane is an ORDERED list of tabs. Two are BUILTIN — Chat
 * and Tasks — present in every conversation, direct lines included, and
 * non-removable, but they share ONE draggable order with the viewer's custom
 * tabs: a builtin can be dragged after a custom one, it just can't be closed.
 * Custom tabs (attachment previewers, panel surfaces) are reorderable and
 * removable.
 *
 * Functional tab order and open state are shared by this client's user.
 * Attachment previews and active selection remain conversation-bound. The persistence and active
 * selection store lives in `place-memory.ts` and resets on logout.
 *
 * PURE module: no React, no localStorage, no queries. The persistence half
 * lives in `place-memory.ts` (identity-bound, wiped on logout) and consumes
 * {@link parseTab} / {@link normalizeTabs} to validate and repair what it
 * hydrates. Splitting the two keeps this logic — identity, dedupe, reorder,
 * builtin-repair, active-fallback — unit-testable without touching storage.
 */

/**
 * One tab. A flat discriminated union so the persisted unit is a single ordered
 * list that self-describes each element — builtins and customs sit side by side
 * in one order.
 */
export type Tab =
  | { kind: "chat" }
  | { kind: "tasks" }
  | {
      kind: "attachment";
      /** The attachment's id — the whole identity of an attachment tab, so
       *  opening the same attachment twice de-dupes to one tab
       *  ({@link addTab}). */
      attachmentId: string;
      /** Presented tab label. */
      filename: string;
      /** Decides how the previewer renders the body (md vs sandboxed html). */
      contentType: string;
    }
  | {
      kind: "panel";
      /** Which conversation surface this tab hosts. The kind IS the whole
       *  identity, so one conversation holds at most one tab per surface and
       *  re-adding focuses the open one ({@link addTab}). Nothing about the
       *  surface's CONTENT is carried here — rows, permissions and filters are
       *  always re-read from their own authorities on mount. */
      panel: PanelTabKind;
    };

/** The conversation surfaces that can be opened as a tab. */
export type PanelTabKind = "attachments" | "threads";

/** Derived from a total map rather than a bare array, so adding a
 *  {@link PanelTabKind} without extending validation is a type error instead of
 *  a silently-rejected persisted tab. */
const PANEL_TAB_KINDS = {
  attachments: true,
  threads: true,
} as const satisfies Record<PanelTabKind, true>;

function isPanelTabKind(value: unknown): value is PanelTabKind {
  // `Object.hasOwn`, not `in`: `in` walks the prototype chain, so a persisted
  // `"toString"` would validate as a surface and mount a blank tab.
  return typeof value === "string" && Object.hasOwn(PANEL_TAB_KINDS, value);
}

/** The builtin kinds, in their default leading order. */
const BUILTIN_KINDS = ["chat", "tasks"] as const;
type BuiltinKind = (typeof BUILTIN_KINDS)[number];

/** The default order for a conversation with no persisted layout. */
export const DEFAULT_TABS: readonly Tab[] = [{ kind: "chat" }, { kind: "tasks" }];

/** True for tabs the viewer may close. Builtins are non-removable; custom tabs
 *  (attachment previewers and panel surfaces) can be removed from a
 *  conversation. */
export function isRemovable(tab: Tab): boolean {
  return tab.kind === "attachment" || tab.kind === "panel";
}

/**
 * A tab's stable key — React key, active-tab selector, and dedupe identity.
 * Namespaced so a builtin can never collide with an attachment id or a panel
 * kind.
 */
export function tabKey(tab: Tab): string {
  switch (tab.kind) {
    case "attachment":
      return `attachment:${tab.attachmentId}`;
    case "panel":
      return `panel:${tab.panel}`;
    default:
      return tab.kind;
  }
}

/** The key of the leading tab in an order. */
export function firstTabKey(tabs: readonly Tab[]): string {
  const first = tabs[0];
  return first ? tabKey(first) : "chat";
}

/**
 * Repair an ordered list into a valid one: drop duplicate keys (first wins),
 * then guarantee BOTH builtins are present — a missing builtin is appended in
 * chat-then-tasks order. Existing builtin positions are preserved, so a viewer
 * who dragged Tasks ahead of Chat keeps that order; only genuinely absent
 * builtins are added. The one invariant every read/write upholds: exactly one
 * Chat and one Tasks always exist, and they cannot be dragged out of existence.
 */
export function normalizeTabs(tabs: readonly Tab[]): Tab[] {
  const out: Tab[] = [];
  const seen = new Set<string>();
  for (const tab of tabs) {
    const key = tabKey(tab);
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(tab);
  }
  for (const kind of BUILTIN_KINDS) {
    if (!seen.has(kind)) out.push({ kind } as Tab);
  }
  return out;
}

/**
 * Add a custom tab, or focus the existing one — the DATA half of open-or-focus.
 * De-dupes on identity ({@link tabKey}): re-adding an attachment already open
 * returns the list UNCHANGED (same array reference) so a caller can cheaply
 * detect "already there → just make it active" and never grows a duplicate.
 * A genuinely new tab is appended after the current last tab.
 */
export function addTab(tabs: readonly Tab[], next: Tab): Tab[] {
  const nextKey = tabKey(next);
  if (tabs.some((t) => tabKey(t) === nextKey)) return tabs as Tab[];
  return [...tabs, next];
}

/**
 * Remove the tab with this key. Builtins are non-removable — a key naming Chat
 * or Tasks (or an absent key) returns the list unchanged. Removing a custom tab
 * is non-destructive: it only drops the tab from this conversation's layout;
 * the attachment and its data are untouched.
 */
export function removeTab(tabs: readonly Tab[], key: string): Tab[] {
  const target = tabs.find((t) => tabKey(t) === key);
  if (!target || !isRemovable(target)) return tabs as Tab[];
  return tabs.filter((t) => tabKey(t) !== key);
}

/**
 * Move the tab at `fromIndex` to `toIndex` (drag-to-reorder). Indices are into
 * the WHOLE list — builtins reorder alongside customs, they just can't be
 * removed. Both ends are clamped into range; an out-of-range or no-op move
 * returns the list unchanged.
 */
export function reorderTabs(tabs: readonly Tab[], fromIndex: number, toIndex: number): Tab[] {
  const from = Math.trunc(fromIndex);
  const to = Math.trunc(toIndex);
  if (from === to || from < 0 || from >= tabs.length || to < 0 || to >= tabs.length) {
    return tabs as Tab[];
  }
  const out = [...tabs];
  const [moved] = out.splice(from, 1);
  if (!moved) return tabs as Tab[]; // unreachable after bounds check; satisfies the type
  out.splice(to, 0, moved);
  return out;
}

/**
 * Resolve the active tab key against the tabs that actually exist. Returns
 * `desiredKey` when it names a present tab, else the FIRST tab in order — so
 * removing the active tab, or a stale key, lands on the leading tab rather than
 * a blank pane. `null`/absent desired also resolves to first (a fresh open).
 */
export function resolveActiveTabKey(
  tabs: readonly Tab[],
  desiredKey: string | null | undefined,
): string {
  if (desiredKey && tabs.some((t) => tabKey(t) === desiredKey)) return desiredKey;
  return firstTabKey(tabs);
}

/**
 * The tab `delta` steps from the active one, wrapping at both ends, so the
 * keyboard cycles the strip in its visible order.
 */
export function adjacentTabKey(tabs: readonly Tab[], activeKey: string, delta: number): string {
  if (tabs.length === 0) return activeKey;
  const index = tabs.findIndex((t) => tabKey(t) === activeKey);
  const base = index < 0 ? 0 : index;
  const next = (((base + delta) % tabs.length) + tabs.length) % tabs.length;
  return tabKey(tabs[next]!);
}

/**
 * Validate one persisted tab. Any shape that doesn't round-trip the {@link Tab}
 * contract is dropped rather than trusted — same discipline as `parseLayout` in
 * place-memory.ts, and the reason a hand-edited or schema-drifted localStorage
 * value can never mint a broken tab.
 */
export function parseTab(value: unknown): Tab | null {
  if (!value || typeof value !== "object") return null;
  const v = value as {
    kind?: unknown;
    attachmentId?: unknown;
    filename?: unknown;
    contentType?: unknown;
    panel?: unknown;
  };
  if (v.kind === "chat" || v.kind === "tasks") return { kind: v.kind };
  if (v.kind === "attachment") {
    if (typeof v.attachmentId !== "string" || v.attachmentId.length === 0) return null;
    if (typeof v.filename !== "string" || v.filename.length === 0) return null;
    if (typeof v.contentType !== "string") return null;
    return {
      kind: "attachment",
      attachmentId: v.attachmentId,
      filename: v.filename,
      contentType: v.contentType,
    };
  }
  if (v.kind === "panel") {
    if (!isPanelTabKind(v.panel)) return null;
    return { kind: "panel", panel: v.panel };
  }
  return null;
}

/**
 * Parse a persisted ordered list: keep the well-formed tabs, then
 * {@link normalizeTabs} to restore the builtins invariant. A non-array (corrupt
 * or missing) value yields the default order. Never returns null — the result
 * always carries both builtins.
 */
export function parseTabs(value: unknown): Tab[] {
  if (!Array.isArray(value)) return normalizeTabs([]);
  const parsed = value.map(parseTab).filter((t): t is Tab => t !== null);
  return normalizeTabs(parsed);
}

/** True when an order is the untouched default (both builtins, default order,
 *  no customs) — lets persistence skip storing a conversation the viewer never
 *  customized. */
export function isDefaultTabs(tabs: readonly Tab[]): boolean {
  return (
    tabs.length === DEFAULT_TABS.length &&
    tabs.every((t, i) => tabKey(t) === tabKey(DEFAULT_TABS[i] as Tab))
  );
}

export type { BuiltinKind };
