import type {
  ChannelAttentionMode,
  ChannelAttentionModeRowDto,
  FavoriteRowDto,
} from "@silverhand/shared";
import { channelRowBadge, type RowBadge } from "@/features/attention/attention-mode";

/** Reveal and paint from the same badge verdict, including directed unread under mute. */
export function favoriteRowBadge(
  item: Pick<FavoriteRowDto, "roster_policy" | "unread_count">,
  attention: ChannelAttentionModeRowDto | undefined,
) {
  return channelRowBadge({
    effectiveMode: item.roster_policy === "open" ? attention?.effective_mode : "all",
    positionalUnread: item.unread_count ?? 0,
    directedUnread: attention?.directed_unread_count ?? 0,
  });
}

export function directRowBadge(unread: number) {
  return channelRowBadge({ effectiveMode: "all", positionalUnread: unread, directedUnread: 0 });
}

/** The unread badge a channel row shows, or `null`. Extracted so the collapsed
 *  poke-out reveal filter (`WorkspaceChannels`) paints from the EXACT same predicate
 *  the row does: a row is revealed while collapsed iff the badge it would show is
 *  `loud`, and `loud` is precisely what a row contributes to its workspace's wake
 *  badge (`channelRowBadge`'s Σ invariant). Folds in the member gate — a browse-only
 *  public row (non-member, or an unknown row on a stale payload) has no unread of its
 *  own — and the `dm_channel`'s unconditional `'all'` tier. Missing/unresolved
 *  attention leaves `effective_mode` undefined, which `channelRowBadge` renders as
 *  nothing: a loading batch reveals no rows rather than fabricating unread. */
export function channelNodeBadge(
  channel: { is_member?: boolean; roster_policy: "open" | "fixed" },
  attention: ChannelAttentionModeRowDto | undefined,
  unreadCount: number,
): RowBadge | null {
  if (channel.is_member !== true) return null;
  const effectiveMode: ChannelAttentionMode | undefined =
    channel.roster_policy === "open" ? attention?.effective_mode : "all";
  return channelRowBadge({
    effectiveMode,
    positionalUnread: unreadCount,
    directedUnread: attention?.directed_unread_count ?? 0,
  });
}
