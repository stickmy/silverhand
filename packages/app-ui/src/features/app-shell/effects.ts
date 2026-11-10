import { getPlatform } from "@/platform";
import { useEffect, useState } from "react";
import { useViewerWorkspacesSync } from "@/features/workspaces/hooks";
import { useDirectPairsSync } from "@/features/direct-pairs/hooks";
import { usePresenceSync } from "@/features/users/presence";
import { useAgentSync } from "@/features/agents/model/agent-sync";
import { useUserSync } from "@/features/users/user-sync";
import { useHostConnectionSync } from "@/features/computers/model/host-connection-sync";
import { useConversationIngestor } from "@/features/realtime/conversation-ingestor";
import { useViewerUnreadIngestor } from "@/features/realtime/viewer-unread-ingestor";
import { cleanupAllComposerDrafts } from "@/features/messages/composer-draft-cache";

// App-shell orchestration effects, lifted out of `RegisteredAppShell` so the
// shell stays a thin composition root. Each hook preserves the exact call
// position it had inline, so the overall hook order is unchanged — these are
// pure boundary extractions, no behavior change.

/**
 * Global, mount-once realtime/visibility sync. These hooks are independent of any
 * view or selection state and are mounted ONCE post-auth so their app-scope WS
 * consumers stay live regardless of which surface is reading their caches.
 * See the per-hook docs at their definitions.
 */
export function useAppShellGlobalSync(): void {
  // Keep the viewer's workspace list live on cross-workspace membership/role
  // changes (added/removed/role-changed), incl. "added to your first
  // workspace". See docs/viewer-visibility-sync-v1.md.
  useViewerWorkspacesSync();
  // Same shape for the viewer's WORKSPACE-LESS lines (#task:2035): a
  // `direct_pair`'s only carrier is the viewer scope, and the case that forces
  // this to be mount-once is a stranger's FIRST DM — a row no cache holds yet,
  // arriving while the viewer may be looking at any section.
  useDirectPairsSync();
  // Keep human online/offline presence live — the global `app`-scope
  // `user_presence` deltas patch the shared ['presence'] cache regardless of
  // which surface reads it. See docs/human-presence-v1.md.
  usePresenceSync();
  // Keep every AgentDto (name/description/avatar/runtime/model) live — the
  // global `agent_changed` event invalidates the ['agents'] query tree
  // regardless of who made the edit. See #task:1178.
  useAgentSync();
  // Same contract for the other actor kind: the global `user_changed` event
  // reseats every cache that carries a human's display name / avatar, and the
  // viewer's own record additionally gets a fresh `GET /api/me`. This is the
  // only notice a client gets when a profile is edited on ANOTHER machine.
  // See #task:1702 / #task:1706.
  useUserSync();
  // Keep owner-scoped host availability/list/detail caches live on daemon
  // attach/detach and WS replay gaps. The event is an invalidation signal; the
  // authoritative state is still `/api/hosts/:id/status`.
  useHostConnectionSync();
  // Stage-2 central conversation ingestor (#task:1255) — the single `onWsFrame`
  // consumer for `conversation:<id>`-scope main-stream cache writes. Views are
  // pure readers of the caches it fills. See docs/renderer-event-ingestion-v1.md.
  useConversationIngestor();
  // S1 viewer-unread badge dispatcher (#task:1284) — the sole `onWsFrame`
  // consumer for message-variant `unread_updated`, patching per-workspace /
  // global / favorites badges in place from the enriched absolute payload.
  // See docs/projection-recovery-s1-unread-v1.md.
  useViewerUnreadIngestor();
}

/**
 * Shutdown overlay step — shown when the main process starts its graceful quit
 * sequence. Steps arrive via IPC; null means not shutting down.
 */
export function useShutdownProgress(): string | null {
  const [shutdownStep, setShutdownStep] = useState<string | null>(null);
  useEffect(() => {
    return getPlatform().shell?.onShutdownProgress((step) => setShutdownStep(step));
  }, []);
  return shutdownStep;
}

/** Flush any pending composer drafts to disk on window unload. */
export function useComposerDraftCleanup(): void {
  useEffect(() => {
    const cleanup = (): void => cleanupAllComposerDrafts();
    window.addEventListener("beforeunload", cleanup);
    return () => {
      window.removeEventListener("beforeunload", cleanup);
    };
  }, []);
}

/**
 * macOS dock badge sync — the app-level face of the viewer's attention badge
 * roll-up (`AttentionBadgeRollupDto.total`, #task:1870), NOT the raw unread total.
 * A muted workspace that still bumped the dock would defeat muting, and the
 * roll-up is the one place the three tiers are already folded in.
 *
 * The total arrives pre-summed and is maintained as `Σ by_workspace` by its
 * carrier, so this does not re-derive it: one fact, one computation. Notification
 * unread (mentions / agent_start_failed) stays out — a different concept that
 * would double-count mentions which are also unread messages.
 */
export function useDockBadge(total: number | undefined): void {
  useEffect(() => {
    void getPlatform().notifications?.setBadgeCount(total ?? 0);
  }, [total]);
}

/**
 * Global Cmd+\ shortcut to toggle the browse sidebar. Ignores modifier-混入 combos
 * (ctrl/alt) so it never hijacks other chords, and ignores Cmd+Shift+\ too: that
 * was the second (channel) column's chord until #task:1889 merged the two into one
 * tree, and letting it fall through to the same toggle would keep a retired chord
 * silently alive.
 *
 * `enabled` scopes the chord to the section that actually owns this browse sidebar
 * (chat). Activity has no browse sidebar — its visible toggle is hidden — so the
 * keyboard path must share that same boundary, otherwise the chord silently mutates
 * the (hidden) chat sidebar preference from Activity and the layout jumps on return
 * to chat (#task:1394 review, Cindy). We gate registration rather than
 * early-returning inside the handler so a disabled section also leaves the chord
 * free (no stray `preventDefault`).
 */
export function useLayoutKeyboardShortcuts(
  toggleWorkspaceSidebar: () => void,
  enabled: boolean,
): void {
  useEffect(() => {
    if (!enabled) return;
    const handleKeyDown = (event: KeyboardEvent): void => {
      if (!event.metaKey || event.ctrlKey || event.altKey || event.shiftKey) return;
      if (event.code !== "Backslash") return;
      event.preventDefault();
      toggleWorkspaceSidebar();
    };
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [enabled, toggleWorkspaceSidebar]);
}

/**
 * App menu's "Settings" item (Cmd+,) — main process focuses the window and
 * pushes this; renderer just navigates. `toSettings` is stable across renders.
 */
export function useSettingsMenuNav(toSettings: () => void): void {
  useEffect(() => {
    return getPlatform().shell?.onNavigateSettings(() => toSettings());
  }, [toSettings]);
}
