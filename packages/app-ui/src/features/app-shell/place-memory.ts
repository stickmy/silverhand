/**
 * Per-workspace "last viewed channel" persistence.
 *
 * The map lives in a module-level singleton hydrated from localStorage at
 * import time. Mutations write through to localStorage immediately so
 * the next launch restores the place even if the app crashes between
 * routes.
 *
 * Multi-human audit 2026-06-03 #task:857 P0.7. The map is
 * **identity-bound** — when a remote-backend viewer logs out, the next
 * session's human must NOT inherit the previous viewer's last-visited
 * channel. {@link clearPlaceMemory} wipes both the disk value AND the
 * in-process Map so app.tsx's restore effect sees an empty slate.
 *
 * Besides the last workspace and last channel per workspace, we also remember
 * which ARM the Chat rail was last on ({@link getLastChatPlace}) so leaving the
 * rail and coming back restores a DIRECT line as faithfully as a workspace
 * channel, plus the last **secondary-pane layout per channel**
 * ({@link lastLayoutByChannel})
 * so switching away from a channel and back restores its threads-list /
 * thread / side overlay instead of dropping to a bare channel, plus a
 * per-channel **explicit-close watermark**
 * ({@link closeWatermarkByChannel}) that lets an unread thread auto-surface
 * without immediately re-surfacing a batch the user just closed. All
 * place-memory state is wiped together on logout.
 *
 * Lives in `lib/` (not `app.tsx`) so `useLogout` in
 * `features/session/hooks.ts` can import it without taking a circular
 * dependency on the App module.
 */

import type { ChannelLayout } from "@/router";
import type { MessageSeq } from "@silverhand/shared";
import { decodeMessageSeq } from "@silverhand/shared";
import {
  DEFAULT_TABS,
  firstTabKey,
  isDefaultTabs,
  normalizeTabs,
  parseTabs,
  resolveActiveTabKey,
  tabKey,
  type Tab,
} from "../conversation-shell/model/conversation-tabs";

export const LAST_CHANNEL_KEY = "silverhand:layout.last-channel-by-workspace";
export const LAST_WORKSPACE_KEY = "silverhand:layout.last-workspace";
export const LAST_CHAT_PLACE_KEY = "silverhand:layout.last-chat-place";
export const LAST_LAYOUT_KEY = "silverhand:layout.last-layout-by-channel";
export const CLOSE_WATERMARK_KEY = "silverhand:layout.close-watermark-by-channel";
export const CONVERSATION_TABS_KEY = "silverhand:layout.conversation-tabs";
export const FUNCTIONAL_TABS_KEY = "silverhand:layout.functional-tabs";
/** Set once the default panels (Threads, Attachments) have been offered to this
 *  viewer. Layouts stored before they became defaults get them added once; a
 *  later close is kept. */
export const DEFAULT_PANELS_SEEDED_KEY = "silverhand:layout.default-panels-seeded";

function loadStringValue(key: string): string | null {
  try {
    const value = localStorage.getItem(key);
    return typeof value === "string" && value.length > 0 ? value : null;
  } catch {
    return null;
  }
}

function saveStringValue(key: string, value: string): void {
  try {
    localStorage.setItem(key, value);
  } catch {
    // Quota / disabled storage — fall back to in-memory only.
  }
}

function loadStringMap(key: string): Map<string, string> {
  try {
    const raw = localStorage.getItem(key);
    if (!raw) return new Map();
    const obj = JSON.parse(raw) as Record<string, string>;
    if (!obj || typeof obj !== "object") return new Map();
    return new Map(
      Object.entries(obj).filter(([k, v]) => typeof k === "string" && typeof v === "string"),
    );
  } catch {
    return new Map();
  }
}

function saveStringMap(key: string, map: Map<string, string>): void {
  try {
    localStorage.setItem(key, JSON.stringify(Object.fromEntries(map)));
  } catch {
    // Quota / disabled storage — fall back to in-memory only.
  }
}

function loadNumberMap(key: string): Map<string, number> {
  try {
    const raw = localStorage.getItem(key);
    if (!raw) return new Map();
    const obj = JSON.parse(raw) as Record<string, unknown>;
    if (!obj || typeof obj !== "object") return new Map();
    return new Map(
      Object.entries(obj).filter(
        (entry): entry is [string, number] =>
          typeof entry[0] === "string" && typeof entry[1] === "number",
      ),
    );
  } catch {
    return new Map();
  }
}

function saveNumberMap(key: string, map: Map<string, number>): void {
  try {
    localStorage.setItem(key, JSON.stringify(Object.fromEntries(map)));
  } catch {
    // Quota / disabled storage — fall back to in-memory only.
  }
}

export const lastChannelByWorkspace = loadStringMap(LAST_CHANNEL_KEY);
let lastWorkspaceId = loadStringValue(LAST_WORKSPACE_KEY);

export function getLastWorkspaceId(): string | null {
  return lastWorkspaceId;
}

export function recordLastWorkspace(workspaceId: string): void {
  if (lastWorkspaceId === workspaceId) return;
  lastWorkspaceId = workspaceId;
  saveStringValue(LAST_WORKSPACE_KEY, workspaceId);
}

export function persistLastChannel(): void {
  saveStringMap(LAST_CHANNEL_KEY, lastChannelByWorkspace);
}

/**
 * WHICH ARM of the Chat rail the viewer was last on (#task:2042).
 *
 * The rail's restorable selection is one union, deliberately not one id: a
 * `workspace_channel` place carries NO coordinate here because the workspace and
 * channel arms already have their own memory above ({@link recordLastWorkspace} /
 * {@link lastChannelByWorkspace}), and a second copy of a coordinate is a second
 * thing that can disagree with the first. A `direct_pair` place is the only arm
 * that owns a coordinate, because nothing else stores one: #task:2035 keeps a
 * workspace-less line out of the workspace-keyed memory on purpose, and that gate
 * stays — this is a placement-aware selection authority beside it, not a
 * widening of it.
 *
 * A recorded pair id is NOT a claim that the line is still reachable. It is
 * re-verified against the viewer's own `direct-pairs` list at restore
 * (`resolveChatPlaceRestore`), so a pair that was deleted, or whose counterpart
 * the viewer is no longer an endpoint across from, degrades to the workspace arm
 * instead of restoring a dead route from stale memory.
 */
export type ChatPlace =
  | { kind: "workspace_channel" }
  | { kind: "direct_pair"; conversationId: string };

/** Validate one persisted place. Any shape that doesn't round-trip the
 *  {@link ChatPlace} contract is dropped rather than trusted — same discipline as
 *  {@link parseLayout}, and the reason a hand-edited value cannot become a route. */
function parseChatPlace(value: unknown): ChatPlace | null {
  if (!value || typeof value !== "object") return null;
  const place = value as { kind?: unknown; conversationId?: unknown };
  if (place.kind === "workspace_channel") return { kind: "workspace_channel" };
  if (place.kind === "direct_pair") {
    return typeof place.conversationId === "string" && place.conversationId.length > 0
      ? { kind: "direct_pair", conversationId: place.conversationId }
      : null;
  }
  return null;
}

let lastChatPlace: ChatPlace | null = (() => {
  const raw = loadStringValue(LAST_CHAT_PLACE_KEY);
  if (raw === null) return null;
  try {
    return parseChatPlace(JSON.parse(raw));
  } catch {
    return null;
  }
})();

/** The last Chat-rail place, or null when nothing has been recorded (cold
 *  start / after logout). */
export function getLastChatPlace(): ChatPlace | null {
  return lastChatPlace;
}

/** Record the Chat rail's place, persisting through to disk. No-ops when the
 *  place is unchanged so an unrelated route change doesn't thrash localStorage. */
export function recordLastChatPlace(place: ChatPlace): void {
  if (
    lastChatPlace &&
    lastChatPlace.kind === place.kind &&
    (place.kind === "workspace_channel" ||
      (lastChatPlace.kind === "direct_pair" &&
        lastChatPlace.conversationId === place.conversationId))
  )
    return;
  lastChatPlace = place;
  saveStringValue(LAST_CHAT_PLACE_KEY, JSON.stringify(place));
}

/**
 * Per-channel secondary-pane layout. Hydrated from localStorage at import
 * time and written through on every change so the layout survives both a
 * channel switch and an app restart. Values are validated on load —
 * anything that doesn't match the {@link ChannelLayout} shape is dropped
 * rather than trusted.
 */
function loadLayoutMap(key: string): Map<string, ChannelLayout> {
  try {
    const raw = localStorage.getItem(key);
    if (!raw) return new Map();
    const obj = JSON.parse(raw) as Record<string, unknown>;
    if (!obj || typeof obj !== "object") return new Map();
    const out = new Map<string, ChannelLayout>();
    for (const [k, v] of Object.entries(obj)) {
      if (typeof k !== "string") continue;
      const parsed = parseLayout(v);
      if (parsed) out.set(k, parsed);
    }
    return out;
  } catch {
    return new Map();
  }
}

/** Validate one persisted layout entry. Returns null for any shape that
 *  doesn't round-trip the {@link ChannelLayout} contract. */
function parseLayout(value: unknown): ChannelLayout | null {
  if (!value || typeof value !== "object") return null;
  const layout = value as { kind?: unknown; rootId?: unknown; id?: unknown };
  switch (layout.kind) {
    case "none":
      return { kind: "none" };
    case "threads-list":
      return { kind: "threads-list" };
    case "thread":
      return typeof layout.rootId === "string" && layout.rootId.length > 0
        ? { kind: "thread", rootId: layout.rootId }
        : null;
    case "side":
      return typeof layout.id === "string" && layout.id.length > 0
        ? { kind: "side", id: layout.id }
        : null;
    default:
      return null;
  }
}

export const lastLayoutByChannel = loadLayoutMap(LAST_LAYOUT_KEY);

function persistLastLayout(): void {
  try {
    localStorage.setItem(LAST_LAYOUT_KEY, JSON.stringify(Object.fromEntries(lastLayoutByChannel)));
  } catch {
    // Quota / disabled storage — fall back to in-memory only.
  }
}

/** Read a channel's remembered layout, or null when none is stored. */
export function getChannelLayout(channelId: string): ChannelLayout | null {
  return lastLayoutByChannel.get(channelId) ?? null;
}

/**
 * Record a channel's secondary-pane layout, persisting through to disk.
 * No-ops when the layout is byte-identical to what's already stored so we
 * don't thrash localStorage on every unrelated route change.
 */
export function recordChannelLayout(channelId: string, layout: ChannelLayout): void {
  const prev = lastLayoutByChannel.get(channelId);
  if (prev && JSON.stringify(prev) === JSON.stringify(layout)) return;
  lastLayoutByChannel.set(channelId, layout);
  persistLastLayout();
}

/**
 * Per-channel explicit-close watermark (#task:1082). When the user
 * explicitly closes a channel's secondary pane, we stamp the seq of the
 * unread pointer they just dismissed. The channel-open precedence
 * ({@link resolveChannelOpen}) only auto-surfaces an unread thread when
 * its pointer `seq` exceeds this watermark — so a just-closed batch isn't
 * yanked back, but a genuinely newer reply still re-surfaces.
 */
export const closeWatermarkByChannel = loadNumberMap(CLOSE_WATERMARK_KEY);

/** Read a channel's explicit-close watermark, or null when none is set.
 *
 *  #task:1267 §3.4: this read is the renderer's SOLE runtime decode boundary —
 *  the raw `number` stored in localStorage crosses back into typed memory here,
 *  so it is validated into a `MessageSeq` through the shared `decodeMessageSeq`
 *  rather than blindly cast. A corrupt stored value (external tampering — the
 *  numbers we WRITE are always valid MessageSeq) is rejected to null ("no
 *  watermark", so unread still surfaces), never laundered through as a real seq. */
export function getCloseWatermark(channelId: string): MessageSeq | null {
  const raw = closeWatermarkByChannel.get(channelId);
  if (raw === undefined) return null;
  try {
    return decodeMessageSeq(raw);
  } catch {
    return null;
  }
}

/**
 * Stamp a channel's explicit-close watermark, persisting through to disk.
 * Monotonic — a smaller seq never lowers an existing watermark, and a
 * byte-identical value no-ops to avoid thrashing localStorage.
 */
export function recordCloseWatermark(channelId: string, seq: MessageSeq): void {
  const prev = closeWatermarkByChannel.get(channelId);
  if (prev !== undefined && prev >= seq) return;
  closeWatermarkByChannel.set(channelId, seq);
  saveNumberMap(CLOSE_WATERMARK_KEY, closeWatermarkByChannel);
}

/** Legacy mixed orders retain attachment positions; functional entries are shared. */
function loadTabsMap(key: string): Map<string, Tab[]> {
  try {
    const raw = localStorage.getItem(key);
    if (!raw) return new Map();
    const obj = JSON.parse(raw) as Record<string, unknown>;
    if (!obj || typeof obj !== "object") return new Map();
    const out = new Map<string, Tab[]>();
    for (const [k, v] of Object.entries(obj)) {
      if (typeof k !== "string" || !Array.isArray(v)) continue;
      const tabs = parseTabs(v);
      // Only keep a conversation the viewer actually customized; a stored
      // default is redundant with the get-time fallback.
      if (!isDefaultTabs(tabs)) out.set(k, tabs);
    }
    return out;
  } catch {
    return new Map();
  }
}

export const tabsByConversation = loadTabsMap(CONVERSATION_TABS_KEY);

function readFunctionalTabs(): Tab[] {
  try {
    const raw = localStorage.getItem(FUNCTIONAL_TABS_KEY);
    if (raw)
      return normalizeTabs(parseTabs(JSON.parse(raw)).filter((tab) => tab.kind !== "attachment"));
  } catch {
    // Invalid storage falls back to the default.
  }
  return [...DEFAULT_TABS];
}

function loadFunctionalTabs(): Tab[] {
  const tabs = readFunctionalTabs();
  if (loadStringValue(DEFAULT_PANELS_SEEDED_KEY)) return tabs;

  const missing = DEFAULT_TABS.filter((d) => !tabs.some((t) => tabKey(t) === tabKey(d)));
  const seeded = [...tabs, ...missing];
  saveStringValue(DEFAULT_PANELS_SEEDED_KEY, "1");
  if (missing.length > 0) {
    try {
      localStorage.setItem(FUNCTIONAL_TABS_KEY, JSON.stringify(seeded));
    } catch {
      // Quota / disabled storage: the in-memory order still carries the panels.
    }
  }
  return seeded;
}

let functionalTabs = loadFunctionalTabs();
const activeTabByConversation = new Map<string, string>(
  Array.from(tabsByConversation).flatMap(([id, tabs]) => {
    const first = tabs[0];
    return first?.kind === "attachment" ? [[id, tabKey(first)] as const] : [];
  }),
);
const tabListeners = new Set<() => void>();
let tabsVersion = 0;
export const getConversationTabsVersion = (): number => tabsVersion;
export function subscribeConversationTabs(listener: () => void): () => void {
  tabListeners.add(listener);
  return () => {
    tabListeners.delete(listener);
  };
}
function notifyConversationTabs(): void {
  tabsVersion += 1;
  for (const listener of tabListeners) listener();
}

function persistConversationTabs(): void {
  try {
    localStorage.setItem(
      CONVERSATION_TABS_KEY,
      JSON.stringify(Object.fromEntries(tabsByConversation)),
    );
    localStorage.setItem(FUNCTIONAL_TABS_KEY, JSON.stringify(functionalTabs));
  } catch {
    // Quota / disabled storage — fall back to in-memory only.
  }
}

/** Replace functional slots with the shared order; file slots remain conversation-bound. */
export function getConversationTabs(conversationId: string): Tab[] {
  const local = tabsByConversation.get(conversationId) ?? [];
  const remaining = [...functionalTabs];
  const tabs: Tab[] = [];
  for (const tab of local) {
    if (tab.kind === "attachment") tabs.push(tab);
    else {
      const next = remaining.shift();
      if (next) tabs.push(next);
    }
  }
  return [...tabs, ...remaining];
}

export function getConversationActiveTab(conversationId: string): string {
  const tabs = getConversationTabs(conversationId);
  const key = resolveActiveTabKey(
    tabs,
    activeTabByConversation.get(conversationId) ?? firstTabKey(functionalTabs),
  );
  // Remember the initial choice too, so a later shared reorder does not switch it.
  activeTabByConversation.set(conversationId, key);
  return key;
}

/** Every active selection belongs to its conversation, including functional tabs. */
export function recordConversationActiveTab(conversationId: string, key: string): void {
  if (!getConversationTabs(conversationId).some((tab) => tabKey(tab) === key)) return;
  if (activeTabByConversation.get(conversationId) === key) return;
  activeTabByConversation.set(conversationId, key);
  notifyConversationTabs();
}

/** Persist shared functional order/open state and this conversation's file positions. */
export function recordConversationTabs(conversationId: string, tabs: Tab[]): void {
  const normalized = normalizeTabs(tabs);
  const nextFunctional = normalized.filter((tab) => tab.kind !== "attachment");
  const prev = tabsByConversation.get(conversationId);
  const changed =
    JSON.stringify(functionalTabs) !== JSON.stringify(nextFunctional) ||
    JSON.stringify(prev ?? []) !== JSON.stringify(isDefaultTabs(normalized) ? [] : normalized);
  if (!changed) return;
  functionalTabs = nextFunctional;
  if (isDefaultTabs(normalized)) tabsByConversation.delete(conversationId);
  else tabsByConversation.set(conversationId, normalized);
  for (const [id, key] of activeTabByConversation) {
    activeTabByConversation.set(id, resolveActiveTabKey(getConversationTabs(id), key));
  }
  persistConversationTabs();
  notifyConversationTabs();
}

/**
 * Wipe both the disk value and the in-process Map. Called from
 * `useLogout` so the next signed-in human starts with a blank place
 * memory.
 */
export function clearPlaceMemory(): void {
  lastWorkspaceId = null;
  lastChatPlace = null;
  lastChannelByWorkspace.clear();
  lastLayoutByChannel.clear();
  closeWatermarkByChannel.clear();
  tabsByConversation.clear();
  functionalTabs = [...DEFAULT_TABS];
  activeTabByConversation.clear();
  notifyConversationTabs();
  try {
    localStorage.removeItem(LAST_WORKSPACE_KEY);
    localStorage.removeItem(LAST_CHANNEL_KEY);
    localStorage.removeItem(LAST_CHAT_PLACE_KEY);
    localStorage.removeItem(LAST_LAYOUT_KEY);
    localStorage.removeItem(CLOSE_WATERMARK_KEY);
    localStorage.removeItem(CONVERSATION_TABS_KEY);
    localStorage.removeItem(FUNCTIONAL_TABS_KEY);
  } catch {
    // Quota / disabled storage — Maps are already empty.
  }
}
