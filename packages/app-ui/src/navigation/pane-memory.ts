import { parsePanes, writePanes, type Panes } from "@/navigation/panes";

/**
 * The panes each conversation was left with, so opening it again restores its
 * layout. Stored as the conversation URL's pane query, read back through the
 * same parser as a URL.
 */
export const PANE_MEMORY_KEY = "silverhand:layout.panes-by-channel";

const panesByConversation: Map<string, string> = (() => {
  try {
    const raw = localStorage.getItem(PANE_MEMORY_KEY);
    const obj: unknown = raw ? JSON.parse(raw) : null;
    if (!obj || typeof obj !== "object") return new Map();
    return new Map(
      Object.entries(obj).filter(
        (entry): entry is [string, string] => typeof entry[1] === "string",
      ),
    );
  } catch {
    return new Map();
  }
})();

/** A conversation's remembered panes, or none when nothing is stored. */
export function rememberedPanes(conversationId: string): Panes {
  const raw = panesByConversation.get(conversationId);
  return raw === undefined ? [] : parsePanes(new URLSearchParams(raw), conversationId);
}

/** Record a conversation's open panes; an empty list is kept, so a deliberate
 *  close survives re-entry. */
export function rememberPanes(conversationId: string, panes: Panes): void {
  const params = new URLSearchParams();
  writePanes(params, panes, conversationId);
  const raw = params.toString();
  if (panesByConversation.get(conversationId) === raw) return;
  panesByConversation.set(conversationId, raw);
  try {
    localStorage.setItem(PANE_MEMORY_KEY, JSON.stringify(Object.fromEntries(panesByConversation)));
  } catch {
    // Quota / disabled storage: memory only.
  }
}

/** Forget every layout; the next signed-in viewer starts blank. */
export function clearPaneMemory(): void {
  panesByConversation.clear();
  try {
    localStorage.removeItem(PANE_MEMORY_KEY);
  } catch {
    // Disabled storage: nothing persisted.
  }
}
