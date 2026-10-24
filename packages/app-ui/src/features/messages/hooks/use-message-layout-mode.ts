import { useSyncExternalStore } from "react";

/**
 * How chat messages are arranged horizontally.
 *
 * - `'user-right'`: current user's messages flush right, others left
 *   (Slack/iMessage convention).
 * - `'all-left'`: every message left-aligned (Discord / log-style). Default
 *   with a fine pointer; touch devices (phone, iPad, Duo) default to
 *   `'user-right'`, the convention of every messaging app there. The
 *   screenshot export always renders all-left, so a shared image reads as a
 *   neutral log rather than baking in the exporter's personal alignment.
 *
 * Owned by Settings > Appearance (`MessageAlignmentControl`), persisted to
 * localStorage. A MODULE store rather than per-hook `useState` for two
 * reasons: the reader is `MessageItem`, so there is one subscriber per
 * rendered row and a per-instance `storage` listener meant N DOM listeners;
 * and the writer is a different surface entirely, so the switch has to reach
 * already-mounted rows in the same window — the `storage` event does not fire
 * on the writing window, which is why this preference used to need a manual
 * reload.
 */
export type MessageLayoutMode = "user-right" | "all-left";

/** Presentation order for the Settings control — the shared list so the
 *  picker cannot drift from the type. Labels/icons stay in the consumer. */
export const MESSAGE_LAYOUT_MODES: ReadonlyArray<MessageLayoutMode> = ["all-left", "user-right"];

/** Same `silverhand:layout.*` family as the other renderer-local layout
 *  preferences (`sidebar-section`, `workspace-sidebar-visible`, …) — the key
 *  used to be dot-separated, which forked the convention for no reason
 *  (shiki 2026-08-06 「统一吧,这个只有我在用」; the rename resets the stored
 *  choice once, back to the default). */
const STORAGE_KEY = "silverhand:layout.message-mode";
function defaultMode(): MessageLayoutMode {
  return typeof window !== "undefined" && window.matchMedia("(pointer: coarse)").matches
    ? "user-right"
    : "all-left";
}

function parse(raw: string | null): MessageLayoutMode {
  return raw === "all-left" || raw === "user-right" ? raw : defaultMode();
}

function readStored(): MessageLayoutMode {
  try {
    return parse(localStorage.getItem(STORAGE_KEY));
  } catch {
    // private mode / no storage — in-memory state still wins
    return defaultMode();
  }
}

let mode: MessageLayoutMode = readStored();
const listeners = new Set<() => void>();

function emit(): void {
  for (const listener of listeners) listener();
}

function onStorage(event: StorageEvent): void {
  if (event.key !== STORAGE_KEY) return;
  const next = parse(event.newValue);
  if (next === mode) return;
  mode = next;
  emit();
}

/** Bound lazily so importing this module has no side effect, and only once
 *  for the whole app however many rows are mounted. Covers writes from
 *  ANOTHER window; same-window writes go through `setMessageLayoutMode`. */
function subscribe(listener: () => void): () => void {
  if (listeners.size === 0 && typeof window !== "undefined") {
    window.addEventListener("storage", onStorage);
  }
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
    if (listeners.size === 0 && typeof window !== "undefined") {
      window.removeEventListener("storage", onStorage);
    }
  };
}

function getSnapshot(): MessageLayoutMode {
  return mode;
}

export function setMessageLayoutMode(next: MessageLayoutMode): void {
  if (next === mode) return;
  mode = next;
  try {
    localStorage.setItem(STORAGE_KEY, next);
  } catch {
    // private mode / quota — the in-memory switch still applies this session
  }
  emit();
}

export function useMessageLayoutMode(): MessageLayoutMode {
  return useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
}
