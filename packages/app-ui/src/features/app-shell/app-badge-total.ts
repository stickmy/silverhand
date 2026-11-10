import type { DirectPairListItem } from "@silverhand/shared";
import { directRowBadge } from "@/features/attention/row-badges";
import { directLinePairVisibleInSidebar } from "@/features/direct-pairs/direct-line-rows";

/** The direct pairs whose sidebar row shows a loud badge, with that count. */
export function loudDirectPairs(
  pairs: readonly DirectPairListItem[],
): { pair: DirectPairListItem; count: number }[] {
  const loud: { pair: DirectPairListItem; count: number }[] = [];
  for (const pair of pairs) {
    const badge = directRowBadge(pair.unread_count);
    if (directLinePairVisibleInSidebar(pair) && badge?.tier === "loud") {
      loud.push({ pair, count: badge.count });
    }
  }
  return loud;
}

/**
 * The app-level badge: the attention roll-up plus direct messages. The roll-up
 * is summed per workspace and a direct pair has no workspace, so its unread
 * would otherwise never reach the Dock or the menu bar.
 */
export function appBadgeTotal(
  workspaceTotal: number | undefined,
  directPairs: readonly DirectPairListItem[],
): number | undefined {
  const direct = loudDirectPairs(directPairs).reduce((sum, item) => sum + item.count, 0);

  if (workspaceTotal === undefined && direct === 0) {
    return undefined;
  }
  return (workspaceTotal ?? 0) + direct;
}
