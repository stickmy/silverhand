/**
 * `#task:1462` — pure selector for the single app-status banner slot.
 *
 * Two independent signals want the same top-center floating slot:
 *   - backend unreachable (`useBackendUnreachableSnitch`)
 *   - outbound-protocol skew (`describeProtocolSkew`)
 *
 * They are different axes and CAN briefly co-exist — e.g. a skew state is
 * cached, then the server drops; until the next protocol probe turns skew
 * into `unknown`, both signals are live. Rendering two components at one
 * `fixed` coordinate would overlap them, so this selector collapses the
 * two into one model with an explicit priority: an unreachable backend is
 * the more fundamental failure (you can't act on a protocol mismatch you
 * can't even reach), so it wins.
 *
 * Extracted as a pure function so the priority is pinned by a unit test
 * rather than living implicitly in JSX branch order.
 */
import type { ProtocolSkewView } from "./protocol-skew-presentation";

export type AppStatusBannerModel =
  | { kind: "offline" }
  | { kind: "backend_unreachable" }
  | { kind: "protocol_skew"; view: ProtocolSkewView }
  | null;

export function selectAppStatusBanner(input: {
  online: boolean;
  unreachableVisible: boolean;
  skew: ProtocolSkewView | null;
}): AppStatusBannerModel {
  // Priority: offline > backend_unreachable > protocol_skew. Offline explains
  // the other two: while it lasts, requests pause rather than fail.
  if (!input.online) return { kind: "offline" };
  if (input.unreachableVisible) return { kind: "backend_unreachable" };
  if (input.skew) return { kind: "protocol_skew", view: input.skew };
  return null;
}
