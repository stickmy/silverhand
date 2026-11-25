import { confirmLogicalSend } from "@/features/channels/logical-send";
import {
  messagesKey,
  clusterMessagesKey,
  reconcilePendingMessage,
  type MessagesCache,
} from "@/features/channels/message-cache-core";
import { useEffect } from "react";
import { useQueryClient, notifyManager } from "@tanstack/react-query";
import type { QueryClient } from "@tanstack/react-query";
import {
  ATTACHMENT_CHANGED_EVENT,
  MESSAGE_OPTIONS_CHANGED_EVENT,
  MESSAGE_REACTION_CHANGED_EVENT,
  parseConversationScope,
  parseConversationSideScope,
} from "@silverhand/shared";
import type {
  AgentRunState,
  AttachmentChangedEvent,
  ConversationActivityEvent,
  MessageDto,
  MessageEventPayload,
  MessageOptionsChangedEvent,
  MessageProjections,
  MessageReactionChangedEvent,
  ServerFrame,
  TaskChangedEvent,
} from "@silverhand/shared";
import { onWsFrame, eventWsClient } from "@/lib/ws-client";
import {
  bumpAttachmentBlobEpoch,
  bumpAttachmentRevision,
} from "@/features/attachments/attachment-blob-cache";
import { getCurrentUserId } from "@/features/session/current-user";
import {
  appendAgentActivityToCache,
  hasActiveAgentActivityReader,
  reconcileAgentActivityOnSnapshotRequired,
} from "@/features/agents/hooks/use-agent-activity-feed";
import {
  applyAgentRunStateToCache,
  hasActiveAgentRunStatesReader,
  invalidateAgentRunStates,
} from "@/features/agents/model/agent-run-state-cache";
import {
  hasActiveChannelThreadsReader,
  invalidateChannelThreadsList,
  invalidateChannelThreadsUnread,
} from "@/features/channels/use-channel-threads";
import {
  hasActiveChannelAttachmentsReader,
  invalidateChannelAttachmentsList,
  invalidateChannelAttachmentSenders,
} from "@/features/channels/use-channel-attachments";
import {
  applyAgentStatusToCache,
  applyClusterMessageToCaches,
  applyMainlineMessageToChannelCaches,
  applyOptionsChangedToMessage,
  applyTaskChangedToMessage,
  applyThreadFollowersMessage,
  applyThreadRootReplyBump,
  hasActiveChannelAgentRuntimeReader,
  hasActiveMainlineReader,
  hasWarmInactiveMainline,
  invalidateThreadFollowers,
  mergeMainlineReplyPreview,
  openClusterReaderRoots,
  openThreadFollowerReaderRoots,
  openThreadRootReaderRoots,
  patchMessageInActiveProjections,
  patchWarmInactiveMessageRow,
  reconcileChannelRuntimeOnSnapshotRequired,
  reconcileChannelMainlineOnSnapshotRequired,
  reconcileClusterOnSnapshotRequired,
  reconcileThreadRootOnSnapshotRequired,
  removeInactiveClusterCache,
  removeInactiveMainlineCache,
  warmInactiveClusterRoots,
} from "@/features/channels/hooks";
import type { ConversationAgentStatusPatch } from "@/features/channels/hooks";
import { applyCommittedReactionFromEvent } from "@/features/channels/reaction-hooks";
import { applyTaskChangedToTaskByNumberCache } from "@/features/tasks/hooks/use-task-by-number";
import {
  dispositionSettle,
  ingestMessageProjections,
  validateMessageProjections,
} from "./projection-registry";

type ReplayCursorEffect = "content-apply" | "cursor-inert";
type ReplayRecoveryEffect = "idempotent" | "authoritative-invalidate" | "side-scope-self-healing";

type ReplaySafetySpec = {
  readonly cursor: ReplayCursorEffect;
  readonly recovery: ReplayRecoveryEffect;
  readonly summary: string;
};

export const REPLAY_SAFETY_CONTRACTS = {
  mainline_row_window: {
    cursor: "content-apply",
    recovery: "idempotent",
    summary:
      "mainline row append / live-gap affordance is id-deduped content; the row loads authoritatively when the window reaches tail",
  },
  mainline_reply_count: {
    cursor: "cursor-inert",
    recovery: "authoritative-invalidate",
    summary:
      "mainline reply badge is a message-derived count; #1263 (c) recovers it authoritatively (no optimistic +1), replay-safe",
  },
  cluster_message_window: {
    cursor: "content-apply",
    recovery: "idempotent",
    summary: "cluster message append is deduped by message id",
  },
  thread_root_reply_count: {
    cursor: "cursor-inert",
    recovery: "authoritative-invalidate",
    summary:
      "thread-root reply count is message-derived; #1263 (c) recovers it authoritatively (no optimistic +1), replay-safe",
  },
  thread_followers_invalidate: {
    cursor: "cursor-inert",
    recovery: "authoritative-invalidate",
    summary: "followers projection refetches from its own source",
  },
  channel_threads_invalidate: {
    cursor: "cursor-inert",
    recovery: "authoritative-invalidate",
    summary: "threads list is message-derived invalidate, not message content",
  },
  task_message_patch: {
    cursor: "content-apply",
    recovery: "idempotent",
    summary:
      "task chip sets an absolute linked_task snapshot on the SOURCE message row (by source_message_id) across active AND warm-inactive caches; a source-backed frame ALWAYS satisfies the floor (displayed/warm → patched, absent row → authoritative-on-paginate vacuous-satisfy), a source-less task is vacuously satisfied",
  },
  options_message_patch: {
    cursor: "content-apply",
    recovery: "idempotent",
    summary:
      "options + current answer are an absolute snapshot SET on the question row by message id across active AND warm-inactive caches; every frame satisfies (absent row → authoritative-on-paginate)",
  },
  reaction_message_patch: {
    cursor: "content-apply",
    recovery: "idempotent",
    summary:
      "reaction patch sets absolute reaction state by message id — the FLOOR-BEARING effect of a reaction frame (its synchronous content-apply gate)",
  },
  reactors_list_refresh: {
    cursor: "cursor-inert",
    recovery: "authoritative-invalidate",
    summary:
      'reactors popover detail list (who-reacted) refreshed by the reaction patch — a FIRE-AND-FORGET, authoritative-on-open detail invalidate. NOT resume-floor-bearing: a reaction frame keeps the SYNCHRONOUS content-apply gate on reaction_message_patch (#1263 hybrid boundary), so unlike a `message` frame this cursor-inert effect is deliberately NOT commit-gated via allSettled — do not "unify" it into the message satisfaction path',
  },
  agent_run_states: {
    cursor: "cursor-inert",
    recovery: "side-scope-self-healing",
    summary: "side scope cache with its own refetch-on-mount/snapshot reconcile",
  },
  channel_agents: {
    cursor: "cursor-inert",
    recovery: "side-scope-self-healing",
    summary: "side scope runtime cache with snapshot reconcile and pending patch buffer",
  },
  agent_activity_feed: {
    cursor: "cursor-inert",
    recovery: "side-scope-self-healing",
    summary: "side scope activity feed merges by row id and has REST cold-start",
  },
  attachment_blob_generation: {
    cursor: "cursor-inert",
    recovery: "side-scope-self-healing",
    summary:
      "side scope blob-URL cache generation; attachment_changed bumps the named id revision (and invalidates the open files-panel list so its size refreshes), snapshot_required coarse-bumps the epoch, both self-heal via no-store refetch of the latest bytes (holds no message content)",
  },
} as const satisfies Record<string, ReplaySafetySpec>;

type ReplaySafetyContracts = typeof REPLAY_SAFETY_CONTRACTS;
type ReplayEffectId = keyof ReplaySafetyContracts;
type ReplayEffectByCursor<C extends ReplayCursorEffect> = {
  [K in ReplayEffectId]: ReplaySafetyContracts[K]["cursor"] extends C ? K : never;
}[ReplayEffectId];

function contentEffectApplied(
  _effect: ReplayEffectByCursor<"content-apply">,
  applied: boolean,
): boolean {
  return applied;
}

function cursorInertEffect(_effect: ReplayEffectByCursor<"cursor-inert">): void {}

export const MESSAGE_STREAM_KIND_CONTRACTS = {
  message: [
    "mainline_row_window",
    "mainline_reply_count",
    "cluster_message_window",
    "thread_root_reply_count",
    "thread_followers_invalidate",
    "channel_threads_invalidate",
  ],
  task_changed: ["task_message_patch"],
  [MESSAGE_REACTION_CHANGED_EVENT]: ["reaction_message_patch", "reactors_list_refresh"],
  [MESSAGE_OPTIONS_CHANGED_EVENT]: ["options_message_patch"],
} as const satisfies Record<string, readonly ReplayEffectId[]>;

type MessageStreamKind = keyof typeof MESSAGE_STREAM_KIND_CONTRACTS;

export const SIDE_KIND_CONTRACTS = {
  agent_activity: ["agent_run_states"],
  agent_status: ["channel_agents"],
  conversation_activity: ["agent_activity_feed"],
  [ATTACHMENT_CHANGED_EVENT]: ["attachment_blob_generation"],
} as const satisfies Record<string, readonly ReplayEffectId[]>;

type SideKind = keyof typeof SIDE_KIND_CONTRACTS;

function asMessageStreamKind(kind: string): MessageStreamKind | null {
  return kind in MESSAGE_STREAM_KIND_CONTRACTS ? (kind as MessageStreamKind) : null;
}

function asSideKind(kind: string): SideKind | null {
  return kind in SIDE_KIND_CONTRACTS ? (kind as SideKind) : null;
}

/**
 * Stage-2 central conversation ingestor (#task:1255).
 *
 * The single App-level consumer of conversation realtime frames across BOTH
 * transport scopes — the message stream `conversation:<id>` (message /
 * message_reaction_changed / task_changed) and the side channel
 * `conversation-side:<id>` (agent_status / agent_activity /
 * conversation_activity). It owns the cache writes that used to
 * live in each view's `onWsFrame` handler, so views can be
 * pure readers (zero `onWsFrame`) — the precondition Stage-2 P2 needs to drop the
 * orphan gate (gate-(b)). See docs/renderer-event-ingestion-v1.md and
 * docs/event-ws-transport-v1.md §4.2.
 *
 * P1a + P1b + P1c scope: the main-stream caches (old
 * `useChannelMessages.onEvent`), the thread cluster / root / followers caches
 * (old `useThreadCluster` / `useThreadRoot` / `useThreadFollowers` `onEvent`s),
 * the channel Threads list (old `useChannelThreads.onEvent`), `agent_activity`
 * + run-states reconcile (old `useAgentRunStates.onEvent` + the shared bridge
 * side-effect), `agent_status` (old `useChannelAgents.onEvent`), and
 * `conversation_activity` (old `AgentActivityTimeline.onEvent`). The message-
 * window and product-projection caches are all driven HERE; the one exception is
 * `useConversationMembers` — a side-scope, invalidate-only consumer that keeps
 * its own `onWsFrame` (cursor-inert: no message-window cache, just a members
 * refetch on `conversation_members_changed`). `ChannelView`'s
 * read cursor is NOT here by design — read advancement stays on the read axis
 * (`useMarkMessageChannelRead`), driven by the mainline cache, not frame arrival
 * (read-inert, #1244/#1251).
 *
 * SCOPE PARTITION (#task:1255 P2 step-3). The recovery classes are split across
 * two disjoint transport scopes. Routing goes through the SHARED parsers only
 * (`parseConversationScope` / `parseConversationSideScope`) — never a fuzzy
 * `startsWith('conversation')` — so a side frame never reaches a message-stream
 * apply and vice-versa. Side kinds publish to `conversation-side:<id>` (backend
 * cutover = Alice P2 step-2); a `snapshot_required` reconciles only the caches of
 * the scope it arrived on. The message-derived Threads list / thread followers
 * STAY on the message stream (their liveness rides the `message` kind).
 *
 * PER-TARGET READER GATE (behavior equivalence — P1 = prep, not correctness):
 * each write ran in the old code ONLY while its hook was mounted. The ingestor
 * preserves that — mainline writes gate on {@link hasActiveMainlineReader};
 * cluster / root / follower writes enumerate the OPEN reader roots (those with a
 * live observer) and apply per-root. A scope kept subscribed by INTEREST alone
 * (e.g. an Activity ThreadPanel-only pane: thread reader, no mainline reader)
 * must NOT bootstrap/patch a target with no REST snapshot behind it — a blanket
 * gate would mis-fire (Cindy blocker #1 / its cluster-axis dual, Alice).
 *
 * These reader gates govern where CONTENT is WRITTEN — they are NOT lifted by P2
 * / #1261 (P3's interest-registry adds cold-start seeding so a readerless scope
 * can hold content at all). P2 splits the single cursor into two water-marks: a
 * `received-water` (wire continuity / gap detection, always advances on arrival)
 * ⊥ an `applied-water` (the resume floor). #1261 promotes the applied-water from
 * "content-apply only" to SATISFACTION: the floor advances once EVERY per-frame
 * effect is settled-fulfilled — a synchronous content apply (in-window append /
 * cluster append·patch / reaction·task chip patch / live-gap affordance), an
 * AUTHORITATIVE derived-invalidate that COMMITS (reply count #1263 (c) /
 * followers / threads list — an invalidate INITIATES recovery, so the floor waits
 * for its refetch, initiated ≠ committed), or a warm-inactive window REMOVED so it
 * cold-starts. A rejected refetch pins the floor (the transport re-drives on the
 * next resume). This REFINES the old orphan-gate floor-pin rather than preserving
 * it: a frame with no active reader and NO warm cache is COLD-ABSENT → its effect
 * set is empty → vacuously satisfied (the floor advances), because its content
 * cold-starts authoritatively on next open and advancing past it can't strand a
 * warm cache — the #1251 hole needs a warm DISPLAYABLE cache, which cold-absent
 * lacks. A readerless-but-WARM window is instead removed (never vacuously
 * satisfied — that would serve post-advance stale rows, Cindy B1). So the reader
 * gate governs cache-writes, not the floor. (`reaction` is the one carve-out —
 * a pure idempotent by-id patch with no derived-invalidate, so it keeps the
 * synchronous content-apply gate. `task_changed` is also a by-id patch but writes
 * its ABSOLUTE chip snapshot to active AND warm-inactive caches and then always
 * satisfies for a source-backed frame — displayed/warm patched, off-window
 * vacuous-satisfy — so it leaves no stale warm cache and never pins the floor on
 * an out-of-window source row.) The `markSatisfied` / `commitSnapshotBaseline` water-
 * mark WRITERS live in Alice's transport seam (`ws-client.ts`, step-4); this
 * ingestor is their sole content-driven CALLER — it fires `markSatisfied` off the
 * per-frame `Promise.allSettled` satisfaction join and drives
 * `commitSnapshotBaseline` off the snapshot-reconcile settle join (see
 * `ingestConversationFrame` below). See §3.1/§5.
 *
 * Read-inert by construction: it never advances the viewer read cursor or
 * writes notification read state — read advancement stays visibility/intent-
 * driven (#task:1244, #task:1251). The applied-water it drives here is the
 * transport RESUME floor, a distinct axis from the viewer read cursor; the
 * `received-water` still auto-advances transport-side (`acceptEventFrame`).
 */

// Scope routing goes through the SHARED parsers only (never a fuzzy
// `startsWith('conversation')`, which would let a `conversation-side:` frame
// leak into the message-stream branch). `parseConversationScope` matches
// `conversation:<id>` (message stream: message / message_reaction_changed /
// task_changed);
// `parseConversationSideScope` matches `conversation-side:<id>` (side channel:
// agent_status / agent_activity / conversation_activity). The two prefixes are
// disjoint by construction (#task:1255 P2 step-3).

/** Build the SATISFACTION disposition for one committed-message payload
 *  (#task:1255 / #task:1263 (c)). Returns the per-effect settle Promises the
 *  caller joins with `Promise.allSettled` to gate `markSatisfied` — all-fulfilled
 *  advances the resume floor once; a rejected refetch pins it (§4.1 / #1261,
 *  commit-gated ≠ dispatch-gated). This is the per-event twin of
 *  {@link ingestMessageStreamSnapshotRequired}: an active reader APPLIES content
 *  (append/gap → resolved) or RECOVERS a derived projection (reply count /
 *  followers / threads list → the invalidate's settle); a warm-but-inactive
 *  CONTENT window is REMOVED, never left to serve post-advance stale rows (Cindy
 *  B1 / #1255 §6); a cold ABSENT target contributes nothing → the frame is
 *  vacuously satisfied (its content cold-starts authoritatively on next open, so
 *  advancing the floor past it can't strand a warm cache — the #1251 hole needs a
 *  warm displayable cache, which cold-absent lacks). Unlike the snapshot handler's
 *  blanket sweep, a live frame touches only its OWN root, so the warm-cluster
 *  removal is SCOPED to that root.
 *
 *  RAW effects only — NEVER call this from two planes directly. Both the WS frame
 *  and the human send ACK are authoritative inputs for the SAME committed message,
 *  so they enter through {@link applyCommittedMessage}, which batches these writes
 *  for atomic observer visibility (count + preview tail + threads-list land in one
 *  notification). Data correctness does NOT depend on running exactly once: every
 *  write here is by-id/absolute (append short-circuits on `cacheHasMessage`, counts
 *  SET an absolute value, previews dedupe by message_id, threads-list overwrites by
 *  root) or an idempotent invalidate. So when the ACK and the WS frame both deliver
 *  this committed message, re-running is safe — the only cost is that the
 *  invalidate-only effects (followers / attachments / no-projection fallback) MAY
 *  refetch once more. That duplicate is deliberately accepted (Cindy + shiki,
 *  #task:1749): a message-level coordination receipt to erase it would trade real
 *  state complexity (per-scope retention aligned to the replay horizon, in-flight
 *  pinning, compare-and-delete release) for a bounded best-effort refetch. If
 *  telemetry ever proves a material amplification, upgrade the specific projection
 *  to an absolute patch or add generic coalescing at the query layer instead of a
 *  message-level receipt. This function itself writes NO transport water —
 *  `markSatisfied` is the WS wrapper's exclusive job (#task:1749). */
function committedMessageEffects(
  qc: QueryClient,
  channelId: string,
  msg: MessageDto,
  projections: MessageProjections | undefined,
): Promise<void>[] {
  const settles: Promise<void>[] = [];
  const push = (eff: Promise<void> | null): void => {
    if (eff) settles.push(eff);
  };

  // S1 (#task:1277): a reply frame carrying an absolute reply_count patch MERGES
  // the authoritative value into the displayed root row + mainline badge (below,
  // via the projection registry) instead of the invalidate→refetch fallback. The
  // patch is EFFECTIVE only when it targets THIS frame's reply root — a root
  // mismatch is a dirty sidecar ("active but no valid patch", Cindy blocker / doc
  // §10 gate #6), so keep the fallback and don't merge the wrong root. SINGLE-POINT
  // validation ({@link validateMessageProjections}) is the anti-corruption boundary:
  // it validates each projection against its own association key ONCE, and BOTH the
  // suppression flag here and the registry dispatch below consume that one result —
  // you cannot suppress rootA's fallback while still merging rootB's dirty patch.
  const validatedProjections = projections
    ? validateMessageProjections(projections, msg)
    : undefined;
  const isPatchedReply = validatedProjections?.thread_root_reply_count != null;
  // #1278: a valid threads_list_entry patch merges/repositions the row in place
  // (registry below) → suppress the blanket threads-list invalidate for this frame.
  const isPatchedThreadsList = validatedProjections?.threads_list_entry != null;

  // MAINLINE — a reply badge is a message-derived count (authoritative-invalidate,
  // cursor-inert; #1277 upgrades it to an absolute merge when a patch rides the
  // frame); a mainline row is an id-deduped content append/gap. A warm-but-
  // inactive mainline (default staleTime:5s) would serve a stale row/count on
  // remount → REMOVE it so the next open cold-starts (coarse, mirroring the
  // snapshot handler: any message frame can touch the window).
  if (hasActiveMainlineReader(qc, channelId)) {
    // A patched reply's badge is merged by the registry below; a non-reply row
    // (append/gap) and the no-patch reply-badge fallback still go through here.
    if (!isPatchedReply) {
      push(applyMainlineMessageToChannelCaches(qc, channelId, msg));
    } else if (msg.thread_root_id !== null) {
      // #task:1745: the count patch suppressed the invalidate that used to be
      // what refreshed the root's inline reply-PREVIEW tail, so patch that tail
      // from this very frame — otherwise the badge counts a reply the lines
      // above it predate, and a root's FIRST reply would never open a tail at
      // all. Cursor-inert and cache-only. It deliberately does NOT fall back to
      // the invalidate this branch exists to suppress: every case it declines
      // (ineligible reply, off-window root, a read model that projects no
      // previews) displays no tail, so a whole-window refetch would buy nothing
      // and would re-add the amplification #task:1277 removed. Hence no
      // satisfaction promise either.
      mergeMainlineReplyPreview(qc, channelId, msg.thread_root_id, msg);
    }
  } else if (hasWarmInactiveMainline(qc, channelId)) {
    push(removeInactiveMainlineCache(qc, channelId));
  }

  // CLUSTERS — each open cluster filters the row to its own root; append is an
  // id-deduped content write. A warm-inactive cluster for THIS msg's root would
  // serve stale on reopen (refetchOnMount:false) → remove it, SCOPED to the root
  // the frame touches (the snapshot handler removes them all; a live frame only
  // affects one).
  for (const rootId of openClusterReaderRoots(qc, channelId)) {
    push(applyClusterMessageToCaches(qc, channelId, rootId, msg));
  }
  for (const rootId of warmInactiveClusterRoots(qc, channelId)) {
    if (msg.thread_root_id === rootId || msg.id === rootId) {
      push(removeInactiveClusterCache(qc, channelId, rootId));
    }
  }

  // THREAD-ROOT reply count — message-derived (authoritative-invalidate, (c)). A
  // warm-inactive thread-root row is NOT removed — mirrors the snapshot handler
  // (useThreadRoot refetches on mount, so a ≤staleTime stale count self-heals).
  // #1277: suppressed when a patch merges the count (registry below).
  if (!isPatchedReply) {
    for (const rootId of openThreadRootReaderRoots(qc, channelId)) {
      push(applyThreadRootReplyBump(qc, channelId, rootId, msg));
    }
  }

  // FOLLOWERS + THREADS-LIST — message-derived invalidations (cursor-inert): the
  // settle is fulfilled once the refetch lands, gating the floor advance.
  // FOLLOWERS stay on the invalidate path (P3 `viewer_following` is per-viewer, not
  // yet emitted). THREADS-LIST: #1278 upgrades it to an in-place merge+resort when a
  // valid `threads_list_entry` patch rides the frame (registry below) — suppress the
  // blanket invalidate then; absent/off-page still fall back (off-page via the
  // reader's own pending-fallback, no-patch via this invalidate).
  for (const rootId of openThreadFollowerReaderRoots(qc, channelId)) {
    push(applyThreadFollowersMessage(qc, channelId, rootId, msg));
  }
  if (hasActiveChannelThreadsReader(qc, channelId)) {
    if (!isPatchedThreadsList) push(invalidateChannelThreadsList(qc, channelId));
    // #task:1773: the `followed ∩ unread` filtered sibling is NEVER merged in
    // place (its membership is viewer-specific), so when the All list took the
    // S1 `threads_list_entry` merge INSTEAD of a blanket invalidate, the filtered
    // view must still refetch. The non-patched branch already reached it via the
    // family prefix-invalidate above; this narrow invalidate covers only the
    // merge case. (Reader-gate proxy: the panel mounts BOTH datasets, so an active
    // All reader implies the filtered reader is mounted too.)
    else push(invalidateChannelThreadsUnread(qc, channelId));
  }

  // ATTACHMENTS-LIST — message-derived read model. A row is
  // emitted per unique attachment referenced by a persisted message in this
  // channel, so only frames that carry attachment refs can grow / reorder
  // the list. Gate on `attachments?.length > 0` (a plain reply frame must
  // NOT cause a full-list refetch — the reader must not fan a full-message
  // storm into a projection refetch) AND on an active reader (no work while
  // the panel is closed). Thread replies land here too — the DTO carries
  // the parent channel id. This invalidate is idempotent, so under the
  // ACK+WS dual apply it may run once on each plane (accepted best-effort);
  // it is not once-only per message.
  if (
    msg.attachments &&
    msg.attachments.length > 0 &&
    hasActiveChannelAttachmentsReader(qc, channelId)
  ) {
    push(invalidateChannelAttachmentsList(qc, channelId));
    // The new message may carry a sender not yet in the facet set; refresh
    // the (lazy) sender dropdown source so a first-time sender becomes
    // filterable without reopening the panel.
    push(invalidateChannelAttachmentSenders(qc, channelId));
  }

  // S1 authoritative projection merge (#task:1277): dispatch the frame's VALIDATED
  // absolute projection patches through the reader registry; each displayed surface
  // returns a 4-state disposition (§7) mapped to its settle promise for the
  // satisfaction join. A merge settles synchronously (resolved); a surface with no
  // reader / no loaded row is cold-absent-vacuous (nothing pushed). Mismatched
  // patches were already dropped by `validateMessageProjections`, so a dirty
  // sidecar's wrong root never reaches a reader (the suppressed paths above are its
  // no-patch fallback for the frame's real root).
  if (validatedProjections) {
    for (const d of ingestMessageProjections(qc, channelId, validatedProjections)) {
      push(dispositionSettle(d));
    }
  }

  return settles;
}

/** The shared committed-message apply seam. BOTH the WS `message` frame and the
 *  human send ACK call this for a committed message; it runs the derived cache
 *  convergence ({@link committedMessageEffects}), batching the synchronous writes so
 *  an observer never sees a half-applied 0→1 (count without preview tail or vice-
 *  versa), and returns the settled per-effect results.
 *
 *  There is NO once-only guard: the two entry points may each run the effects for
 *  the same committed message. That is safe by the contract on
 *  {@link committedMessageEffects} — data writes are by-id/absolute/idempotent, so a
 *  second run converges to the same cache; only invalidate-only effects may refetch
 *  once more, a bounded best-effort cost deliberately accepted in #task:1749 (see
 *  that docblock for why a coordination receipt is NOT worth its state complexity).
 *
 *  Callers:
 *  - WS wrapper: `.then` → `markSatisfied` iff every settle fulfilled (the ONLY
 *    place transport water advances). A rejected settle pins the floor so the
 *    transport re-drives on the next resume.
 *  - ACK (`useSendMessage.onSuccess`): swaps the optimistic row to committed, then
 *    calls this and `void`s the result — it never writes transport water, viewer
 *    read cursor, thread-observe, or unread/follow rollup. */
export function applyCommittedMessage(
  qc: QueryClient,
  channelId: string,
  msg: MessageDto,
  projections: MessageProjections | undefined,
): Promise<PromiseSettledResult<void>[]> {
  // `notifyManager.batch`: every synchronous `setQueryData` / invalidate this
  // payload triggers is flushed to observers as ONE notification, so count +
  // preview tail + threads-list absolute merges are atomically visible. The async
  // refetch settles resolve outside the batch and gate the WS floor via the result.
  const settles = notifyManager.batch(() => {
    confirmLogicalSend(qc, msg);
    const key = msg.thread_root_id
      ? clusterMessagesKey(channelId, msg.thread_root_id)
      : messagesKey(channelId);
    qc.setQueryData<MessagesCache | undefined>(key, (data) =>
      reconcilePendingMessage(data, msg, getCurrentUserId()),
    );
    return committedMessageEffects(qc, channelId, msg, projections);
  });
  return Promise.allSettled(settles);
}

/** Apply a live/replayed `agent_activity` frame to the agent-run-states cache
 *  (#task:1255 P1c). Verbatim move of the old shared `ingestConversationEvent`
 *  side-effect, now gated on a mounted run-states reader (a non-terminal event
 *  would otherwise bootstrap an incomplete one-row list with no snapshot). */
function ingestAgentActivity(qc: QueryClient, channelId: string, state: AgentRunState): void {
  if (hasActiveAgentRunStatesReader(qc, channelId)) {
    cursorInertEffect("agent_run_states");
    applyAgentRunStateToCache(channelId, state, qc);
  }
}

/** Apply a live `agent_status` frame to the channel runtime cache (#task:1255
 * P1c), gated on a mounted runtime reader. */
function ingestAgentStatus(
  qc: QueryClient,
  channelId: string,
  patch: ConversationAgentStatusPatch,
): void {
  if (hasActiveChannelAgentRuntimeReader(qc, channelId)) {
    cursorInertEffect("channel_agents");
    applyAgentStatusToCache(qc, channelId, patch);
  }
}

/** Apply a live `conversation_activity` row to its agent's activity feed
 *  (#task:1255 P1c #2). The row self-routes by `agent_id`; we gate on that
 *  agent's feed reader being mounted (no bootstrap with no reader) — the
 *  per-agentId equivalent of the cluster-root enumeration. Read-inert: the feed
 *  is content, never a read/transport cursor (Alice P2 red line). */
function ingestConversationActivity(
  qc: QueryClient,
  channelId: string,
  row: ConversationActivityEvent,
): void {
  const agentId = row.agent_id;
  if (!agentId) return;
  if (hasActiveAgentActivityReader(qc, channelId, agentId)) {
    cursorInertEffect("agent_activity_feed");
    appendAgentActivityToCache(qc, channelId, agentId, row);
  }
}

function ingestTaskChanged(qc: QueryClient, channelId: string, task: TaskChangedEvent): boolean {
  // The task-by-number cache is a SEPARATE projection (the inline `#task:<n>`
  // chip + its hover card), not the message-stream content the resume floor
  // tracks. It is an ABSOLUTE by-id cache write — no request — fired outside
  // the row-patch seam, membership-gated by its OWN cache (no-op / no bootstrap
  // when absent), and NEVER counted in `applied`. Firing it unconditionally (not
  // behind the mainline gate) drops an incidental coupling: a chip must update
  // even when the mainline isn't open.
  applyTaskChangedToTaskByNumberCache(qc, task);
  // The `['task-detail', task_id]` projection is NOT touched here. Its realtime
  // owner is `useTaskDetail` (viewer scope), which covers every conversation the
  // viewer belongs to — a human's detail read gate is conversation membership,
  // so the viewer frame reaches anything the modal may show, including a task
  // whose channel has no mounted stream (#task:1715). One projection, one owner:
  // a second `invalidateQueries` on the same key does not merge — with data
  // already cached it CANCELS the in-flight refetch and restarts it — so keeping
  // a copy here would turn every remote edit into a wasted round trip.
  // The inline task chip lives on the SOURCE message (the one the task was
  // created from) — a first-class task is no longer a message row of its own.
  // A source-less task therefore has NO message-stream row to patch: its only
  // effects are the authoritative-on-open caches above, so the frame is
  // vacuously satisfied (like a cold-absent `message` frame) — return true so
  // the resume floor advances instead of pinning on a patch that can never land.
  if (task.source_message_id === null) return true;
  // Source-backed: write the absolute `linked_task` snapshot to the source row
  // WHEREVER it is cached, then satisfy the floor unconditionally. Three states
  // (Cindy gate) — the chip snapshot is ABSOLUTE (a by-id field SET, not an
  // APPEND), so an in-place patch is authoritative in every one:
  //   1. displayed projection holds it → seam patches → satisfy;
  //   2. only a warm-INACTIVE cache holds it → warm-patch in place (never leave a
  //      displayable stale chip — ws-client forbids satisfying past one) → satisfy;
  //   3. no cache holds it → a future paginate fetches it authoritatively →
  //      VACUOUS satisfy.
  // Old behaviour pinned the floor on an off-window no-op ("harmless") — wrong:
  // a task source commonly sits >1 window back, and pinning strands that seq out
  // of the contiguous satisfied set (ws-client authoritative-on-paginate). We do
  // NOT copy reaction's active-only contract here (its scope is unchanged).
  const patchRow = (message: MessageDto): MessageDto => applyTaskChangedToMessage(message, task);
  const target = {
    channelId,
    messageId: task.source_message_id,
    threadRootId: task.thread_root_id,
  };
  patchMessageInActiveProjections(qc, target, patchRow);
  patchWarmInactiveMessageRow(qc, channelId, task.source_message_id, patchRow);
  return contentEffectApplied("task_message_patch", true);
}

/** Options and answer are an absolute snapshot, so the frame patches the
 *  question wherever it is cached and always satisfies the floor, like
 *  `task_changed`. */
function ingestOptionsChanged(
  qc: QueryClient,
  channelId: string,
  ev: MessageOptionsChangedEvent,
): boolean {
  const patchRow = (message: MessageDto): MessageDto => applyOptionsChangedToMessage(message, ev);
  patchMessageInActiveProjections(
    qc,
    { channelId, messageId: ev.message_id, threadRootId: ev.thread_root_id },
    patchRow,
  );
  patchWarmInactiveMessageRow(qc, channelId, ev.message_id, patchRow);
  return contentEffectApplied("options_message_patch", true);
}

/** WS wrapper over the ordered-plane committed-reaction seam (#task:1753). Its
 *  only transport-specific job is resolving the event's REACTOR into the viewer's
 *  own terminal state — `null` when someone else reacted, so the count moves and
 *  the viewer's flag does not — and then reporting the apply to the resume floor.
 *  The cache convergence itself (absolute row merge across every displayed
 *  projection + reactor-detail refresh) lives in
 *  `applyCommittedReactionFromEvent`. Absolute assignment is THIS plane's
 *  privilege: it is ordered per scope, with replay and a `snapshot_required` gap
 *  fence. The viewer's own write ACK, having no shared ordering axis, converges
 *  through the re-read seam instead; the floor likewise stays WS-exclusive, so no
 *  ACK can ever advance a water-mark.
 *
 *  #1263 hybrid boundary: reaction keeps the SYNCHRONOUS content-apply (boolean)
 *  gate — `applied` is the content-commit signal. The seam's detail invalidate is
 *  fire-and-forget and deliberately NOT commit-gated via allSettled (see
 *  REPLAY_SAFETY_CONTRACTS.reactors_list_refresh). */
function ingestReaction(
  qc: QueryClient,
  channelId: string,
  ev: MessageReactionChangedEvent,
): boolean {
  const byViewer = ev.reactor_kind === "user" && ev.reactor_id === getCurrentUserId();
  return contentEffectApplied(
    "reaction_message_patch",
    applyCommittedReactionFromEvent(qc, {
      // `channelId` comes from the frame SCOPE, the routing authority — not the
      // event's self-declared `conversation_id`.
      target: {
        channelId,
        messageId: ev.message_id,
        threadRootId: ev.thread_root_id,
      },
      emoji: ev.emoji,
      count: ev.count,
      viewerReacted: byViewer ? ev.op === "add" : null,
    }),
  );
}

/** `snapshot_required` on the message-stream scope (`conversation:<id>`):
 *  reconcile every message-derived cache and RETURN the settle Promises so the
 *  caller can gate `commitSnapshotBaseline` on an all-fulfilled
 *  `Promise.allSettled` (§6, apply-gated commit). Active targets invalidate →
 *  refetch (the Promise settles when the refetch lands); a warm-but-inactive
 *  content window — mainline (default `staleTime:5s`) OR cluster
 *  (`refetchOnMount:false`) — is REMOVED so it cold-starts on next open
 *  (synchronous, resolved Promise); an absent target is vacuously satisfied
 *  (nothing pushed). A warm inactive window is NEVER treated as absent: it would
 *  still display post-commit stale rows (`event-ws-transport-v1.md §8`). Empty ⇒
 *  `allSettled([])` is all-fulfilled ⇒ commit (a scope with no live content is
 *  vacuously recoverable). #1255 P2 step-4. */
function ingestMessageStreamSnapshotRequired(qc: QueryClient, channelId: string): Promise<void>[] {
  const settles: Promise<void>[] = [];
  if (hasActiveMainlineReader(qc, channelId)) {
    settles.push(reconcileChannelMainlineOnSnapshotRequired(qc, channelId));
  } else if (hasWarmInactiveMainline(qc, channelId)) {
    // Warm-but-inactive mainline (default staleTime:5s) would serve stale rows
    // on a remount within the window → remove it so the next open cold-starts
    // (§6), same treatment as a warm inactive cluster. Never left as
    // absent/vacuous (that commits the floor over displayable stale content).
    settles.push(removeInactiveMainlineCache(qc, channelId));
  }
  for (const rootId of openClusterReaderRoots(qc, channelId)) {
    settles.push(reconcileClusterOnSnapshotRequired(qc, channelId, rootId));
  }
  // Warm-but-inactive clusters (refetchOnMount:false) can't self-heal via
  // invalidate → remove them so the next open cold-starts (§6).
  for (const rootId of warmInactiveClusterRoots(qc, channelId)) {
    settles.push(removeInactiveClusterCache(qc, channelId, rootId));
  }
  // Open thread-root rows refetch their authoritative reply count — the covering
  // recovery that releases a backfill reply bump that pinned the floor (§4.1).
  for (const rootId of openThreadRootReaderRoots(qc, channelId)) {
    settles.push(reconcileThreadRootOnSnapshotRequired(qc, channelId, rootId));
  }
  for (const rootId of openThreadFollowerReaderRoots(qc, channelId)) {
    settles.push(invalidateThreadFollowers(qc, channelId, rootId));
  }
  // The channel Threads list is message-derived (a reply bumps a cluster / turns
  // a plain root into a thread) → stays on the message stream.
  if (hasActiveChannelThreadsReader(qc, channelId)) {
    settles.push(invalidateChannelThreadsList(qc, channelId));
  }
  // The channel Attachments list is also message-derived. A
  // snapshot_required covers gap recovery — the missed window may have
  // referenced attachments we don't know about, so invalidate + settle
  // whenever the panel reader is active (no `msg.attachments` gate here:
  // snapshot_required carries no payload).
  if (hasActiveChannelAttachmentsReader(qc, channelId)) {
    settles.push(invalidateChannelAttachmentsList(qc, channelId));
    settles.push(invalidateChannelAttachmentSenders(qc, channelId));
  }
  return settles;
}

/** `snapshot_required` on the side-channel scope (`conversation-side:<id>`):
 *  reconcile the side caches (agent-run-states / channel agents / agent-activity
 *  feeds). The side scope has NO content-apply seam — its floor auto-advances
 *  transport-side — so this fires reconciles but never returns a commit signal
 *  or touches a cursor. #1255 P2 step-4 (agent-activity item-2 gap now closed). */
function ingestSideSnapshotRequired(qc: QueryClient, channelId: string): void {
  if (hasActiveAgentRunStatesReader(qc, channelId)) {
    void invalidateAgentRunStates(qc, channelId);
  }
  if (hasActiveChannelAgentRuntimeReader(qc, channelId)) {
    void reconcileChannelRuntimeOnSnapshotRequired(qc, channelId);
  }
  // Agent-activity feeds are per-agentId; invalidate the whole conversation
  // prefix so every mounted feed refetches (item-2 gap, §4.1 / P1c #2).
  void reconcileAgentActivityOnSnapshotRequired(qc, channelId);
  // A side-scope gap may have dropped an `attachment_changed` whose id the
  // `snapshot_required` cannot name (#task:1769). Coarse-bump the blob epoch so
  // every displayed preview refetches the latest bytes; the precise per-id
  // revision bust only covers the no-gap path.
  bumpAttachmentBlobEpoch();
}

/**
 * Route one realtime frame into the channel main-stream + thread caches. Pure
 * (takes the `QueryClient` explicitly) so it is unit-testable without rendering.
 *
 * Scope-partitioned (#1255 P2 step-3): message-stream kinds ride
 * `conversation:<id>`, side-channel kinds ride `conversation-side:<id>`. Routing
 * uses the shared parsers only — the two prefixes are disjoint, so a side frame
 * never reaches a message-stream apply and vice-versa.
 *
 * DUAL-WATER FLOOR (#1255 P2 step-4 / #1263 (c)). For the content-gated
 * message-stream scope this is the SOLE content-driven writer of the transport
 * applied-water (resume floor):
 *  - a `message` `event` → run the shared apply seam
 *    ({@link applyCommittedMessage}, built on {@link committedMessageEffects}) and,
 *    once its `Promise.allSettled` join is
 *    all-fulfilled, `markSatisfied(scope, seq, epoch)` once (#1261: content applies
 *    + committed derived-invalidates + warm-removes + cold-absent vacuous all
 *    satisfy; a rejected refetch pins the floor);
 *  - a `reaction` `event` → a pure idempotent content patch, so the synchronous
 *    `applied` gate stands: a real patch → `markSatisfied` once, an off-window
 *    no-op → none (false-positive forbidden, §4.1);
 *  - a `task_changed` `event` → an ABSOLUTE by-id chip snapshot written to the
 *    source row across active AND warm-inactive caches, then satisfy
 *    UNCONDITIONALLY for a source-backed frame: displayed/warm → patched;
 *    off-window (no cache holds it) → authoritative-on-paginate VACUOUS satisfy
 *    (never pin the floor on a source row that legitimately sits windows back);
 *    a source-less task is vacuously satisfied too. Unlike reaction it does NOT
 *    keep the active-only no-op-pins gate — an absolute patch leaves no stale
 *    warm cache, so satisfying is safe (ws-client displayable-stale invariant);
 *  - `snapshot_required` → collect the reconcile settle Promises and, once the
 *    `Promise.allSettled` join is all-fulfilled, `commitSnapshotBaseline(scope,
 *    epoch)` once (apply-gated commit, §6). A rejected settle leaves the floor
 *    pinned (the transport re-drives on the next resume / snapshot).
 * The side scope has no content-apply seam — its floor auto-advances in the
 * transport (`acceptEventFrame`), so the ingestor never calls `markSatisfied` /
 * `commitSnapshotBaseline` for it (Alice's contract).
 */
export function ingestConversationFrame(qc: QueryClient, frame: ServerFrame): void {
  if (frame.type !== "event" && frame.type !== "snapshot_required") return;
  const messageStreamId = parseConversationScope(frame.scope);
  const sideId = parseConversationSideScope(frame.scope);
  if (!messageStreamId && !sideId) return;

  if (frame.type === "snapshot_required") {
    if (messageStreamId) {
      const settles = ingestMessageStreamSnapshotRequired(qc, messageStreamId);
      const { scope, epoch } = frame;
      // Apply-gated commit: advance the floor to the server-stamped baseline
      // ONLY once every reconcile settled fulfilled. A rejected/aborted refetch
      // leaves the floor pinned — the transport re-drives on the next resume.
      void Promise.allSettled(settles).then((results) => {
        if (results.every((r) => r.status === "fulfilled")) {
          eventWsClient.commitSnapshotBaseline(scope, epoch);
        }
      });
    }
    if (sideId) ingestSideSnapshotRequired(qc, sideId);
    return;
  }

  // Message-stream kinds ride `conversation:<id>`. The applied-water advances on
  // SATISFACTION (#1261), split by effect nature:
  //  - `message` carries cursor-inert derived-invalidate effects (reply counts,
  //    followers, threads list, #1263 (c)) alongside content appends, so it gates
  //    `markSatisfied` on a per-frame `Promise.allSettled` — all-fulfilled advances
  //    the floor once (after any refetch COMMITS); a rejected refetch pins it; a
  //    cold-absent frame satisfies vacuously (allSettled([]) is all-fulfilled).
  //  - `reaction` is a pure idempotent content patch (set-by-id, replay-safe)
  //    with no derived-invalidate effect, so it keeps the synchronous `applied`
  //    gate: a real patch advances the floor, an off-window no-op pins it
  //    (harmless on idempotent replay).
  //  - `task_changed` is an ABSOLUTE by-id chip patch written to active AND
  //    warm-inactive caches; a source-backed frame ALWAYS satisfies (displayed/
  //    warm → patched, off-window → authoritative-on-paginate vacuous-satisfy),
  //    so it never pins the floor on a source row that sits windows back. It does
  //    NOT share reaction's no-op-pins gate (an absolute patch leaves no stale
  //    warm cache).
  if (messageStreamId) {
    const kind = asMessageStreamKind(frame.kind);
    if (kind === "message") {
      const payload = frame.payload as MessageEventPayload | undefined;
      const msg = payload?.message;
      if (msg) {
        const { scope, seq, epoch } = frame;
        // The WS frame and the human send ACK share ONE apply seam. There is no
        // once-only guard: if the ACK already applied this committed message, this
        // WS run re-converges the same by-id/absolute writes (a no-op on data) and
        // may re-fire the invalidate-only effects — accepted, #task:1749.
        // `markSatisfied` is WS-exclusive: it is the only transport-water write,
        // gated on every effect settling fulfilled.
        void applyCommittedMessage(qc, messageStreamId, msg, payload?.projections).then(
          (results) => {
            if (results.every((r) => r.status === "fulfilled")) {
              eventWsClient.markSatisfied(scope, seq, epoch);
            }
          },
        );
      }
      return;
    }
    let applied = false;
    if (kind === "task_changed") {
      const task = frame.payload as TaskChangedEvent | undefined;
      if (task?.task_id) applied = ingestTaskChanged(qc, messageStreamId, task);
    } else if (kind === MESSAGE_REACTION_CHANGED_EVENT) {
      const ev = frame.payload as MessageReactionChangedEvent | undefined;
      if (ev?.message_id) applied = ingestReaction(qc, messageStreamId, ev);
    } else if (kind === MESSAGE_OPTIONS_CHANGED_EVENT) {
      const ev = frame.payload as MessageOptionsChangedEvent | undefined;
      if (ev?.message_id) applied = ingestOptionsChanged(qc, messageStreamId, ev);
    }
    if (applied) eventWsClient.markSatisfied(frame.scope, frame.seq, frame.epoch);
    return;
  }

  // Side-channel kinds ride `conversation-side:<id>`. No content-apply seam —
  // the transport auto-advances the side floor; never call markSatisfied here.
  if (!sideId) return;
  const sideKind = asSideKind(frame.kind);
  if (sideKind === "agent_activity") {
    const state = frame.payload as AgentRunState | undefined;
    if (state?.runId) ingestAgentActivity(qc, sideId, state);
  } else if (sideKind === "agent_status") {
    const patch = frame.payload as ConversationAgentStatusPatch | undefined;
    if (patch?.agent_id) ingestAgentStatus(qc, sideId, patch);
  } else if (sideKind === "conversation_activity") {
    const row = frame.payload as ConversationActivityEvent | undefined;
    if (row?.id != null) ingestConversationActivity(qc, sideId, row);
  } else if (sideKind === ATTACHMENT_CHANGED_EVENT) {
    // Creator updated the attachment's bytes in place (#task:1767 / #task:1769).
    const ev = frame.payload as AttachmentChangedEvent | undefined;
    if (ev?.attachment_id) {
      // (1) Bump the named id's blob generation so any displayed preview
      // refetches the latest bytes under `no-store`. No react-query cache holds
      // the blob URL, so this lives in the blob cache, not `qc`.
      bumpAttachmentRevision(ev.attachment_id, ev.content_revision);
      // (2) The files panel row also renders the attachment `size`, which a
      // content update changes; that field lives in the (message-derived)
      // attachments-list query, so invalidate it when the panel is open so the
      // size refreshes alongside the thumbnail. Membership is unchanged (no
      // message added/removed) → senders list is left alone.
      if (hasActiveChannelAttachmentsReader(qc, sideId)) {
        void invalidateChannelAttachmentsList(qc, sideId);
      }
    }
  }
}

export function useConversationIngestor(): void {
  const qc = useQueryClient();
  useEffect(() => {
    return onWsFrame((frame: ServerFrame) => ingestConversationFrame(qc, frame));
  }, [qc]);
}
