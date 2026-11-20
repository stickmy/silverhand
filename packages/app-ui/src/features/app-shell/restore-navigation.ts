/**
 * Route-restore precedence (#task:1300, extracted from App's auto-select
 * effect).
 *
 * When the chat route resolves to a workspace with NO channel selected
 * (bare `/chat/w/:id`, or after a clear), one of several signals decides
 * which channel to open. The precedence — locked across #task:1082 /
 * #task:1145 and mirrored from the original inline effect — is:
 *
 *   1. **honored jump** — a pending {@link MessageJumpTarget} names a channel
 *      that exists in this workspace: go there (Saved → jump must not snap
 *      back to the remembered/default channel). A jump is an EXPLICIT user
 *      intent, so it is the ONE path allowed to land on an archived channel.
 *   2. **remembered channel** — the per-workspace last-viewed channel, unless
 *      it's the just-archived primary (`excludeId`), gone, or now ARCHIVED (an
 *      archived last-viewed channel must never be auto-restored — #task:1413).
 *      Its pane intent is resolved by {@link resolveChannelOpen}: a
 *      fresh unread thread (past the close-watermark) surfaces that thread,
 *      otherwise the remembered panes are restored.
 *   3. **default fallback** — the first active top-level channel, excluding a
 *      just-archived primary still lingering in the stale list.
 *   4. **none** — no workspace / a channel is already selected / no channels /
 *      no fallback: do nothing.
 *
 * AUTO entries (remembered + fallback) share ONE active-channel gate
 * (`activeTopLevel`): archived channels are reachable only through the Archived
 * panel or an explicit jump, never restored/defaulted into. Because every auto
 * trigger — workspace switch, reload, route restore — funnels through this one
 * resolver, that single gate keeps them all from drifting to an archived
 * target (#task:1413).
 *
 * This resolver is PURE: it reads no refs, no module singletons, and performs
 * no navigation. The caller reads the current ref / place-memory / unread
 * state, calls this, and executes the returned action. That keeps the
 * load-bearing precedence unit-testable independent of React/nav wiring,
 * while every side effect stays in the effect.
 */

import type { MessageUnreadPointerDto, MessageSeq } from "@silverhand/shared";
import type { Panes } from "@/navigation/panes";
import { resolveChannelOpen } from "@/features/conversation-shell/model/channel-open";
import { pickDefaultChannel } from "@/features/app-shell/archive-fallback";

export interface RestoreNavigationInputs {
  /** Route workspace id (`null` when the route has no workspace). */
  workspaceId: string | null;
  /** Route channel id — restore only runs when this is `null`. */
  selectedChannelId: string | null;
  /** All channels in the workspace, active + archived. EXPLICIT-CONSUMER ONLY:
   *  the jump branch alone may consult this union (it may reach an archived
   *  channel); the "workspace has any channel" guard also uses it. AUTO branches
   *  (remembered / fallback) MUST NOT — they read `activeTopLevel` so an
   *  archived channel can never be auto-restored (#task:1413). Named `all*` so a
   *  future auto branch reaching for it reads as a smell. */
  allChannels: readonly { id: string }[];
  /** Active top-level channels in sidebar order — the fallback source AND the
   *  active-channel gate for the remembered branch (auto entries never restore
   *  an archived channel). */
  activeTopLevel: readonly { id: string }[];
  /** Channel a pending jump wants to land on, if any. */
  jumpTargetChannelId: string | null;
  /** Just-archived primary to exclude from remembered + fallback (#task:1145). */
  excludeId: string | null;
  /** Per-workspace last-viewed channel (`null` if none remembered). */
  rememberedChannelId: string | null;
  /** Unread pointer for the remembered channel (for {@link resolveChannelOpen}). */
  rememberedUnreadPointer: MessageUnreadPointerDto | undefined;
  /** Explicit-close watermark for the remembered channel. */
  rememberedCloseWatermark: MessageSeq | null;
  /** Remembered panes of the remembered channel. */
  rememberedPanes: Panes | null;
}

export type RestoreNavigation =
  | { kind: "none" }
  /** Navigate to a bare channel URL. `reason` is for tests; the caller
   *  builds the same URL either way. */
  | { kind: "open-channel"; channelId: string; reason: "jump" | "fallback" }
  /** Navigate to the remembered channel restoring its panes. */
  | { kind: "open-remembered"; channelId: string; panes: Panes | null }
  /** Jump to a fresh unread thread in the remembered channel. */
  | { kind: "jump-unread"; channelId: string; messageId: string; threadRootId: string };

export function resolveRestoreNavigation(inputs: RestoreNavigationInputs): RestoreNavigation {
  const {
    workspaceId,
    selectedChannelId,
    allChannels,
    activeTopLevel,
    jumpTargetChannelId,
    excludeId,
    rememberedChannelId,
    rememberedUnreadPointer,
    rememberedCloseWatermark,
    rememberedPanes,
  } = inputs;

  if (!workspaceId) return { kind: "none" };
  if (selectedChannelId) return { kind: "none" };
  if (allChannels.length === 0) return { kind: "none" };

  // The single active-channel gate shared by the AUTO entries (remembered +
  // fallback). An archived channel is in `allChannels` (the union) but NOT here,
  // so no auto path can restore or default into it (#task:1413).
  const isActiveChannel = (id: string): boolean => activeTopLevel.some((s) => s.id === id);

  // 1. A jump asked for a specific channel — honor it instead of the
  //    remembered/default so Saved → jump doesn't snap back. A jump is an
  //    EXPLICIT user intent (Saved / search / Activity deep-link), so it MAY
  //    land on an archived channel — the one path allowed to, hence the full
  //    `allChannels` union here rather than the active gate.
  if (jumpTargetChannelId && allChannels.some((s) => s.id === jumpTargetChannelId)) {
    return { kind: "open-channel", channelId: jumpTargetChannelId, reason: "jump" };
  }

  // 2. Remembered channel — unless it's the just-archived primary, gone, or
  //    now archived. `isActiveChannel` subsumes the existence check AND excludes
  //    an archived last-viewed channel from auto-restore (#task:1413).
  if (
    rememberedChannelId &&
    rememberedChannelId !== excludeId &&
    isActiveChannel(rememberedChannelId)
  ) {
    const intent = resolveChannelOpen(
      rememberedUnreadPointer,
      rememberedCloseWatermark,
      rememberedPanes,
    );
    if (intent.kind === "unread-thread") {
      return {
        kind: "jump-unread",
        channelId: rememberedChannelId,
        messageId: intent.messageId,
        threadRootId: intent.threadRootId,
      };
    }
    return { kind: "open-remembered", channelId: rememberedChannelId, panes: intent.panes };
  }

  // 3. Default: first active top-level, excluding a just-archived primary.
  const fallbackId = pickDefaultChannel(activeTopLevel, excludeId);
  if (!fallbackId) return { kind: "none" };
  return { kind: "open-channel", channelId: fallbackId, reason: "fallback" };
}
