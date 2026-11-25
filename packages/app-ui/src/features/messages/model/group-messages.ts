import type { MessageDto, SystemSubtype } from "@silverhand/shared";
import { type CachedMessage, isCommittedMessage } from "@/features/channels/pending-message";

export const CLUSTERABLE_SUBTYPES: ReadonlySet<SystemSubtype> = new Set<SystemSubtype>([
  "task_create",
  "task_claim",
  "task_assign",
  "task_unclaim",
  "task_status",
  "task_edit",
]);

/** 5-minute window between consecutive activity events to remain in
 *  the same run. Long enough to absorb a normal flurry of task
 *  mutations during active work; short enough that an idle channel's
 *  next-day audit rows form a fresh cluster instead of being glued to
 *  yesterday's. */
export const ACTIVITY_RUN_GAP_MS = 5 * 60 * 1000;

export interface ActivityCluster {
  kind: "cluster";
  /** Stable React key: see module docstring "Cluster identity". */
  id: string;
  messages: MessageDto[];
}

export interface SingleMessage {
  kind: "message";
  message: CachedMessage;
}

export type StreamItem = ActivityCluster | SingleMessage;

export interface GroupMessagesOptions {
  /** When set, the unread separator is shown immediately BEFORE this
   *  message in the rendered stream. Cluster runs that would straddle
   *  this boundary are split into two clusters at the boundary so the
   *  cluster's visual block doesn't span both sides of the read
   *  cursor. Pass `null` (or omit) when no separator is being shown. */
  unreadSeparatorBeforeMessageId?: string | null;
}

/** Whether `m` is a clusterable system message. Real messages (user/agent)
 *  and optimistic pending rows are never clusterable; system messages are
 *  clusterable iff their `metadata.subtype` is in `CLUSTERABLE_SUBTYPES`. A
 *  clusterable row is always a committed server row, so this is a type guard. */
export function isClusterableMessage(m: CachedMessage): m is MessageDto {
  if (!isCommittedMessage(m)) return false;
  if (m.from_kind !== "system") return false;
  const meta = m.metadata as Record<string, unknown> | null;
  if (meta === null) return false;
  const subtype = meta["subtype"];
  if (typeof subtype !== "string") return false;
  return CLUSTERABLE_SUBTYPES.has(subtype as SystemSubtype);
}

/** Group consecutive clusterable system messages into `ActivityCluster`s.
 *  Pure function — running it on the same input array always produces
 *  the same output. See module docstring for the four grouping rules
 *  and the cluster-id stability rationale. */
export function groupMessages(
  messages: readonly CachedMessage[],
  options: GroupMessagesOptions = {},
): StreamItem[] {
  const out: StreamItem[] = [];
  const unreadId = options.unreadSeparatorBeforeMessageId ?? null;
  let run: MessageDto[] = [];

  const flushRun = (): void => {
    if (run.length === 0) return;
    const first = run[0]!;
    out.push({
      kind: "cluster",
      id: `activity-cluster:${first.id}`,
      messages: run,
    });
    run = [];
  };

  for (const m of messages) {
    if (!isClusterableMessage(m)) {
      flushRun();
      out.push({ kind: "message", message: m });
      continue;
    }

    // Clusterable message — try to join the current run.
    if (run.length === 0) {
      run.push(m);
      continue;
    }

    const prev = run[run.length - 1]!;
    const tooFar = m.created_at - prev.created_at > ACTIVITY_RUN_GAP_MS;
    const crossesUnread = unreadId !== null && m.id === unreadId;

    if (tooFar || crossesUnread) {
      flushRun();
    }
    run.push(m);
  }
  flushRun();
  return out;
}
