import type {
  AgentDto,
  AttachmentListItem,
  AttachmentListItemSource,
  UserDto,
} from "@silverhand/shared";
import dayjs from "dayjs";
import { dayLabel } from "./day-label";

/**
 * Pure helpers for the channel Attachments panel: sender resolution and
 * reverse-chronology by-day grouping. Kept out of the panel component so
 * they are unit-tested with fixed clocks and no DOM.
 *
 * Neither helper touches the uploader axis (`attachment.created_by_*`) —
 * sender identity is the message sender captured in `source.from_*_id`;
 * uploader and sender are separate concepts (an agent may upload a blob
 * for a human to send, and vice versa).
 */

export interface AttachmentSenderIdentity {
  /** Best-effort display label. Prefers a resolved `display_name` (trimmed)
   *  over the internal `name`/handle, and falls back to the raw
   *  `from_*_id` when the sender has been removed from the roster — so a
   *  historical row never collapses to a generic `"agent"` / `"Someone"`
   *  string that would strip identity. `null` only for `from_kind==='system'`
   *  (a system-authored row: no identity axis, callers render `System`). */
  label: string | null;
  /** Raw kind of the sender axis — drives avatar branch. */
  kind: AttachmentListItemSource["from_kind"];
  /** Agent id for kind='agent', else null. Passed to avatar lookups. */
  agentId: string | null;
  /** User id for kind='user', else null. Passed to avatar lookups. */
  userId: string | null;
}

/** The sender axis of a canonical reference — the subset of fields the
 *  identity resolver reads. Both an `AttachmentListItemSource` (a list row)
 *  and an `AttachmentSenderFacet` (a filter option) satisfy this, so the
 *  dropdown resolves a sender identically to the row it filters. */
export type AttachmentSenderActor = Pick<
  AttachmentListItemSource,
  "from_kind" | "from_user_id" | "from_agent_id"
>;

/** Resolve a sender actor into a stable display identity.
 *
 *  Contract (agent):    `agent.display_name.trim() || agent.name`
 *                       → falls back to `from_agent_id` when the agent has
 *                         been removed from the roster.
 *  Contract (user):     `user.display_name.trim() || user.name`
 *                       → falls back to `from_user_id` on same terms.
 *  Contract (system):   `{ label: null }` — no identity axis.
 *
 *  Never returns a generic placeholder (`'agent'` / `'Someone'`); a caller
 *  that wants that must apply it explicitly on `label === null`. */
export function resolveSenderIdentity(
  actor: AttachmentSenderActor,
  maps: { agentsById: Map<string, AgentDto>; humansById: Map<string, UserDto> },
): AttachmentSenderIdentity {
  if (actor.from_kind === "agent" && actor.from_agent_id) {
    const a = maps.agentsById.get(actor.from_agent_id);
    const display = a?.display_name?.trim();
    const label = display || a?.name || actor.from_agent_id;
    return {
      label,
      kind: "agent",
      agentId: actor.from_agent_id,
      userId: null,
    };
  }
  if (actor.from_kind === "user" && actor.from_user_id) {
    const u = maps.humansById.get(actor.from_user_id);
    const display = u?.display_name?.trim();
    const label = display || u?.name || actor.from_user_id;
    return {
      label,
      kind: "user",
      agentId: null,
      userId: actor.from_user_id,
    };
  }
  return { label: null, kind: actor.from_kind, agentId: null, userId: null };
}

// ── By-day grouping ──────────────────────────────────────────────────────

export interface AttachmentDayGroup {
  /** Stable key = ISO date (YYYY-MM-DD) in the viewer's local time. Used
   *  as React keys and lookup handles; NEVER shown to the user. */
  key: string;
  /** Human-facing label — `Today` / `Yesterday` for the two most recent
   *  local days, otherwise a locale-formatted date. */
  label: string;
  items: AttachmentListItem[];
}

/**
 * Group attachments by their `source.appeared_at` day boundary in the
 * viewer's local timezone, preserving the input's reverse-chronological
 * order (newest first). Pure: takes `now` via `deps` so
 * tests pin behavior against a fixed clock.
 *
 * Groups are ordered by their first-appearing item (i.e. the same reverse
 * order the input carries), so an infinite-scroll append lands in the
 * right group without a re-sort — a later page can only extend an existing
 * group at its tail, or open a strictly older group at the list tail.
 */
export function groupAttachmentsByDay(
  items: readonly AttachmentListItem[],
  deps: {
    now: number;
  },
): AttachmentDayGroup[] {
  const { now } = deps;
  const groups: AttachmentDayGroup[] = [];
  const byKey = new Map<string, AttachmentDayGroup>();
  for (const item of items) {
    const key = dayjs(item.source.appeared_at).format("YYYY-MM-DD");
    let group = byKey.get(key);
    if (!group) {
      group = {
        key,
        label: dayLabel(item.source.appeared_at, now),
        items: [],
      };
      byKey.set(key, group);
      groups.push(group);
    }
    group.items.push(item);
  }
  return groups;
}
