import { useChannelDetail } from "@/features/workspaces/hooks";
import {
  channelGateFor,
  deriveChannelMembership,
  directPairGate,
  type ChannelGate,
  type MembershipCarrier,
  type MembershipState,
} from "./membership";

export interface ResolvedChannelMembership {
  membership: MembershipState;
  gate: ChannelGate;
}

/**
 * Resolve a channel's per-viewer membership to the fail-closed three-state,
 * shared by `ChannelView` (the primary/side pane) and `App` (for the open
 * `ThreadPanel`) so both consume the SAME authority instead of each collapsing
 * it their own way. The carrier's own `is_member` wins when present (a
 * discovery-union list item or a channel-detail response); a carrier that lacks
 * it — a cross-workspace conversation pane / deep link — is `unknown`, resolved by a
 * channel-detail fetch (a read allowed to anyone who can view the channel). The
 * fetch is `enabled`-gated to the `unknown` case only, so a member hot path
 * (carrier already stamped) fires zero extra network and never flickers.
 *
 * `channelId` null (no channel selected) leaves the fetch disabled → `unknown`.
 *
 * `directPair` switches the whole derivation off (#task:2035): a pair has no
 * membership axis and no channel to fetch detail for, so the caller passes its
 * `read_only` and gets `directPairGate`. The reported `membership` is `'member'`
 * — a PROJECTION of "the viewer is an endpoint" onto this three-state axis for
 * the consumers that only ask "may member-scoped surfaces mount", not an
 * `is_member` inference from a placement axis (the caller states the placement;
 * the row's existence in the viewer's own pair list is the proof).
 */
export function useChannelMembership(
  carrier: MembershipCarrier | null | undefined,
  channelId: string | null,
  directPair?: { readOnly: boolean } | null,
): ResolvedChannelMembership {
  const carrierMembership = deriveChannelMembership(carrier ?? undefined);
  const needsDetail = !directPair && carrierMembership === "unknown" && !!channelId;
  const { data: detail } = useChannelDetail(channelId, needsDetail);
  const membership = needsDetail
    ? deriveChannelMembership(carrier ?? undefined, detail?.is_member)
    : carrierMembership;
  if (directPair) {
    return { membership: "member", gate: directPairGate(directPair.readOnly) };
  }
  return { membership, gate: channelGateFor(membership) };
}
