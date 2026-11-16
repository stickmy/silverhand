// The single Attention eliminate primitive.
// Every read-state clearing path in the renderer reduces to one `observe()`
// call — the backend routes the write back to the source aggregate (stream
// cursor for `stream`, `notifications.read_at` for `notification`), so the
// renderer never owns any read/eliminate state of its own (answerability,
// §5.3). Lives in the Attention bounded context because eliminate is an
// Attention concern; messaging hooks (channel/thread mark-read) delegate here.
import type { ActivityItemDto, ObserveSource, ObserveDto, MessageSeq } from "@silverhand/shared";
import { apiPaths, asReadSeq } from "@silverhand/shared";
import { api } from "@/lib/api";

/** POST the eliminate primitive. One source = a single clear; many = a Mark-all
 *  batch (one transaction server-side — any illegal source fails the WHOLE
 *  request, so callers build sources only from server-validated DTO fields). */
export function postObserve(sources: ObserveSource[]): Promise<ObserveDto> {
  return api<ObserveDto>(apiPaths.attentionObserve, {
    method: "POST",
    body: JSON.stringify({ sources }),
  });
}

// ── Pure source_ref builders ────────────────────────────────────────────────
// §9.10 guards the caller-INTENT axis, not the stream-shape axis: an AUTO path
// (open / visible-live / thread mark-read) MUST pin `at_seq` and never knowingly
// tail-swallow. The builders here are the AUTO-path builders, and they enforce
// that STRUCTURALLY — a thread (root != null) is only ever built `at_seq`, and
// the only `stream_head` builder here hardcodes `thread_root_id: null`. So no
// AUTO caller can emit thread+`stream_head`. (Phase B adds an EXPLICIT builder,
// `explicitStreamClearHead(conv, root|null)`, that knowingly tail-swallows and
// MAY carry a thread root; the backend permits it. The guard is per-builder
// intent, never a blanket renderer-wide ban on thread+stream_head.)

/** Build an AUTO-path stream clear pinned to a resolved position (thread
 *  open-time snapshot head / visible-live seq; or a channel with a held
 *  snapshot). The AUTO / thread mark-read path only ever emits `at_seq` — this
 *  builder is how that caller intent is enforced, independent of any endpoint
 *  rule (the backend no longer rejects thread + `stream_head`).
 *
 *  #task:1267 §3.1 — this is the ONE genuine MessageSeq → ReadSeq conversion in
 *  the renderer. `seq` in is a MessageSeq (the position the viewer has seen, held
 *  as MessageSeq all the way up the mark-read chain); pinning it as `at_seq`
 *  makes it a read cursor, so it is minted through {@link asReadSeq} here — the
 *  single testable bridge. No caller pre-converts; the role change lives only at
 *  this choke point. */
export function streamSourceAtSeq(
  conversationId: string,
  threadRootId: string | null,
  seq: MessageSeq,
): ObserveSource {
  return {
    kind: "stream",
    conversation_id: conversationId,
    thread_root_id: threadRootId,
    position: { kind: "at_seq", seq: asReadSeq(seq) },
  };
}

/** A server-resolved mainline (channel) stream clear to current head. Allowed
 *  ONLY for an explicit/non-auto path (channel open / jump / ambient hover-✓ /
 *  Mark all) which knowingly accepts concurrent tail-swallow (§2). Hardcodes
 *  `thread_root_id: null` — a thread can never reach this builder. */
export function mainlineStreamSourceHead(conversationId: string): ObserveSource {
  return {
    kind: "stream",
    conversation_id: conversationId,
    thread_root_id: null,
    position: { kind: "stream_head" },
  };
}

/** A directed dismiss by explicit `notification_id` (§5.3 source_ref-out) —
 *  NEVER reverse-advances a cursor, NEVER parses an item's render `id`. */
export function notificationSource(notificationId: string): ObserveSource {
  return { kind: "notification", notification_id: notificationId };
}

/** EXPLICIT-path stream clear to the stream's current head — the phase-B
 *  counterpart to the AUTO builders above. The user is clearing a source WITHOUT
 *  opening it (Activity hover-✓ / Mark all read), so there is no seen-seq to pin:
 *  `stream_head` marks the stream read to now and KNOWINGLY tail-swallows a
 *  message landing in the same instant — honest bulk semantics (§2, R3).
 *
 *  UNLIKE `streamSourceAtSeq` / `mainlineStreamSourceHead`, this MAY carry a
 *  thread root: §9.10 guards the caller-INTENT axis (auto pins at_seq), not the
 *  stream-shape axis, so an EXPLICIT thread clear is legal. Reserved for
 *  explicit user clears ONLY — an AUTO path must never reach this builder. */
export function explicitStreamClearHead(
  conversationId: string,
  threadRootId: string | null,
): ObserveSource {
  return {
    kind: "stream",
    conversation_id: conversationId,
    thread_root_id: threadRootId,
    position: { kind: "stream_head" },
  };
}

/**
 * Map ONE Activity item to its eliminate `source_ref` — the single routing
 * chokepoint for §5.3 source_ref-out (#task:1231 phase B). Every clear path
 * (per-row hover-✓, on-open dismiss, Mark all read) funnels through here, so the
 * route lives in exactly one place.
 *
 * Routed by `kind` (the source_ref MECHANISM), NOT `salience` (the DISPLAY
 * tier) — these are DIFFERENT axes. A DM positional unread is `kind:'unread'` but
 * `salience:'directed'` (directed-grade importance, positional mechanism); it
 * still eliminates via the STREAM CURSOR, never a notification. Branching on
 * salience would route DM unread to the notification path, find no
 * `notification_id`, and silently drop the most attention-worthy unread from both
 * hover-✓ and Mark-all. So:
 *   - `agent_failed` → by `notification_id` (a directed dismiss sets that one
 *     notification's `read_at`; it never reverse-advances a cursor, §3).
 *   - `unread` (mainline, thread, OR DM, with or without mentions — the stream
 *     clear also reads its mentions) → an EXPLICIT stream clear to head
 *     (`explicitStreamClearHead`): clearing without opening has no seen-seq to
 *     pin, so `stream_head` marks to now and knowingly tail-swallows (§2, R3).
 *     MAY carry a thread root — explicit thread clears are legal on the §9.10
 *     caller-intent axis, unlike the AUTO `at_seq`-only path.
 *
 * Returns null only for a malformed notification-backed item with no
 * `notification_id` — callers skip a null source rather than emit a bad clear.
 *
 * Written as an EXHAUSTIVE switch on `kind`: a future `ActivityItemKind` (e.g. a
 * v2 notification-backed `task_needs_review`) must declare its eliminate route
 * here. The `never` guard makes TS fail to compile until it does, so a new kind
 * can never silently fall through to a stream clear.
 */
export function activityItemSource(item: ActivityItemDto): ObserveSource | null {
  switch (item.kind) {
    case "agent_failed":
      // Directed: by notification_id; a directed dismiss never moves a cursor.
      return item.notification_id != null ? notificationSource(item.notification_id) : null;
    case "unread":
      // Stream-backed (mainline / thread / DM positional — keyed on kind, not
      // salience: a DM unread is salience:'directed' but still clears here).
      return explicitStreamClearHead(item.conversation_id, item.thread_root_id);
  }
  const _exhaustive: never = item.kind;
  return _exhaustive;
}
