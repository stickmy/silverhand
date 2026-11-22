import type { OnScreen } from "@/navigation/panes";

/**
 * A thread jump lives as long as its thread pane: once the pane leaves the
 * screen, a later plain open of that thread must not inherit the jump (window
 * lock, jump-path mark-read, locate of an unrelated reply). Only a pane that was
 * shown counts, so a jump that is still navigating to its thread survives.
 */
export function threadJumpOutlivedPane(
  target: { channelId: string; threadRootId?: string | null } | null,
  before: OnScreen["threads"],
  after: OnScreen["threads"],
): boolean {
  const root = target?.threadRootId;
  if (!target || !root) return false;
  const shows = (threads: OnScreen["threads"]) =>
    threads.some((t) => t.conversationId === target.channelId && t.rootId === root);
  return shows(before) && !shows(after);
}
