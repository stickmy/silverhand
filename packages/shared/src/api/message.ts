// AUTO-SPLIT from the former monolithic api/dto.ts (#task:963).
// Bounded context: message. Re-exported via api/index.ts → @silverhand/shared.
import { z } from 'zod';
import { RuntimeKindSchema, ConversationRosterPolicySchema, FromKindSchema } from '../db/schema.js';
import {
  MessageAttachmentIdsInputSchema,
  SearchOptionalStringSchema,
  hasMessagePayload,
} from './_zod-helpers.js';
import { AttachmentLabelSchema, AttachmentSnapshotSchema } from './attachment.js';
import { PARTICIPANT_HANDLE_PATTERN } from './name.js';
import { ReactionAggregateDtoSchema } from './reactions.js';
import { LinkedTaskDtoSchema } from './task.js';
import { MessageAuthorDtoSchema } from './message-author.js';
import { ThreadSummaryInvariantSchema } from './thread-summary.js';
import { MessageSeqSchema } from '../seq.js';
import { UnresolvedReferencesSchema } from './message-reference.js';
import { MessageAnswerDtoSchema, MessageOptionsSchema } from './message-options.js';

export const ClientMessageIdSchema = z
  .string()
  .regex(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i)
  .transform((id) => id.toLowerCase());

export const MessageDtoSchema = z.object({
  client_message_id: ClientMessageIdSchema.optional(),
  id: z.string(),
  conversation_id: z.string(),
  from_kind: FromKindSchema,
  from_agent_id: z.string().nullable(),
  from_user_id: z.string().nullable(),
  /** Server-resolved author presentation, or null for `from_kind='system'`.
   *  The `from_kind`/`from_agent_id`/`from_user_id` above stay the RAW
   *  provenance (self-identification, permission logic); `author` is the
   *  resolved DISPLAY projection layered on top. See MessageAuthorDtoSchema. */
  author: MessageAuthorDtoSchema.nullable(),
  content: z.string(),
  metadata: z.record(z.unknown()).nullable(),
  /** Attachment snapshots attached to this message (null when none).
   *  Mirrored from `metadata.attachments` for typed access. */
  attachments: z.array(AttachmentSnapshotSchema).nullable(),
  /** Viewer-scoped reaction aggregates (one per distinct emoji), or null
   *  when the message has no reactions OR the read path did not enrich them.
   *  Lean by design — reactor identities load lazily via
   *  `GET /api/messages/:id/reactions` on hover (docs/reactions-design.md §6).
   *  `viewer_reacted` is relative to the reading principal. */
  reactions: z.array(ReactionAggregateDtoSchema).nullable(),
  seq: MessageSeqSchema,
  created_at: z.number(),
  /** Reply-cluster thread anchor (v43). NULL = root / main-line message;
   *  non-NULL = a reply in the thread rooted at that message id. The thread
   *  is identified by this root id, not a separate conversation. See
   *  docs/thread-reply-cluster-v1.md. */
  thread_root_id: z.string().nullable(),
  /** The task LINKED to this message via its immutable source provenance
   *  (`tasks.source_message_id == this.id`), or null when this message is not a
   *  task's source. Viewer-invariant (this DTO rides the broadcast
   *  `conversation:{id}` scope). A `public`-non-member HTTP read gets null
   *  (boundary strip). Replaces the retired flat `task_*` "message IS task"
   *  projection. See LinkedTaskDtoSchema. */
  linked_task: LinkedTaskDtoSchema.nullable(),
  /** Answer options the author attached (#task:3143); absent from older servers. */
  options: MessageOptionsSchema.nullable().optional(),
  /** The current answer to `options`, or null while unanswered. */
  answer: MessageAnswerDtoSchema.nullable().optional(),
});
export type MessageDto = z.infer<typeof MessageDtoSchema>;

/** S1 projection-level recovery (#task:1276) — authoritative projection patches
 *  the backend attaches to a `message` event so the client can idempotently
 *  MERGE a derived value instead of invalidate→refetch.
 *
 *  VIEWER-INVARIANT ONLY: a `message` event rides the broadcast
 *  `conversation:{id}` scope (one payload fans to every subscriber), so every
 *  field here MUST be identical for all viewers. Per-viewer facts
 *  (e.g. `viewer_following`) are barred — they ride `viewer:{userId}` or fall
 *  back to refetch. All patches carry ABSOLUTE values (never a delta / `+1`) so
 *  they are replay-idempotent. Canonical: docs/projection-recovery-s1-v1.md. */
export const ThreadRootReplyCountPatchSchema = z.object({
  root_id: z.string(),
  /** Authoritative non-system reply count for `root_id`, computed in the SAME
   *  write transaction as the triggering reply insert (never a future-seq
   *  value). Mirrors `replyCountsByRoot` (`from_kind != 'system'`). */
  reply_count: z.number().int().nonnegative(),
});
export type ThreadRootReplyCountPatch = z.infer<typeof ThreadRootReplyCountPatchSchema>;

export const MessageProjectionsSchema = z.object({
  /** Reply-count projection for the replied-to root (P1). Present only when the
   *  triggering message is a reply (`thread_root_id != null`). */
  thread_root_reply_count: ThreadRootReplyCountPatchSchema.optional(),
  /** Threads-list entry projection for the replied-to root (P2,
   *  docs/projection-recovery-s1-v1.md §5/§6). Present only when the triggering
   *  message is a reply. Carries the VIEWER-INVARIANT `ThreadSummaryInvariant`
   *  (no `viewer_following`) plus `root_seq` (the keyset tie-break); the client
   *  merges the invariant fields into the row it already holds and repositions
   *  its page by the documented keyset `(last_reply_at DESC, root_seq DESC)`. NO
   *  insert in P2 v1 — a root not on the client's page falls back to refetch
   *  (off-page insert is deferred to P2.5). The `{ entry }` wrapper reserves room
   *  for future sibling hints without a wire break. */
  threads_list_entry: z.object({ entry: ThreadSummaryInvariantSchema }).optional(),
});
export type MessageProjections = z.infer<typeof MessageProjectionsSchema>;

/** Wire payload of a `message` event (`kind:'message'`). `message` is the
 *  inserted row; `projections` is the optional S1 authoritative-delta sidecar —
 *  ADDITIVE: a client that ignores it falls back to invalidate→refetch, so the
 *  backend can ship this ahead of the renderer consumer. */
export const MessageEventPayloadSchema = z.object({
  message: MessageDtoSchema,
  projections: MessageProjectionsSchema.optional(),
});
export type MessageEventPayload = z.infer<typeof MessageEventPayloadSchema>;

/**
 * HTTP ACK of a HUMAN send — a strict SUPERSET of the broadcast
 * {@link MessageEventPayload}, never a different shape (#task:1749): the
 * committed payload's own fields verbatim, so the renderer's single idempotent
 * apply seam consumes the ACK and the WS echo through the same reader.
 *
 * The extra key exists because an ACK has an audience of exactly one — the
 * actor who made this attempt — while the `message` event is read by everyone in
 * the conversation. So sender-only diagnostics attach HERE and never inside
 * `MessageEventPayload`, which is why the WS frame stays literally
 * `result.committed`. `unresolved_references` below is exactly this pattern: a
 * fact about the attempt (which of this sender's references addressed nobody),
 * carried by the attempt's answer and seen by no one else.
 */
export const MessageSendAckSchema = MessageEventPayloadSchema.extend({
  /** This sender's references that addressed nobody (#task:2021 P4-5-b) — see
   *  {@link UnresolvedReferencesSchema}, whose single declaration this surface
   *  shares with the agent send output. */
  unresolved_references: UnresolvedReferencesSchema,
});
export type MessageSendAck = z.infer<typeof MessageSendAckSchema>;

/** Per-root enriched reply-badge data (#task:1073, P1). Optional sidecar
 *  that augments `reply_counts_by_root` with what a Slack-style inline reply
 *  badge needs beyond a bare count: the most-recent replier identities (for
 *  a stacked-avatar cluster) and the last-reply time. Absent until the
 *  backend populates it — the badge falls back to count-only. Keyed by root
 *  message id. This schema is the canonical reply-badge sidecar contract.
 *
 *  Carries ONLY optimistically-maintainable reply metadata — NOT per-viewer
 *  unread. Unread / first-unread is server-scored and lives in the dedicated
 *  conversation-unread read-model (`ConversationStreamUnreadDto`, `GET
 *  /api/conversations/:id/unread`); the chip reads "N new" from there, never
 *  from this window sidecar (#task:1226). */
export const ReplyBadgeSummarySchema = z.object({
  /** Total non-system replies. Mirrors `reply_counts_by_root` for self-
   *  containment; the count shown still reads `reply_counts_by_root` so the
   *  two never visibly diverge. */
  reply_count: z.number().int().nonnegative(),
  /** Epoch ms of the most recent non-system reply. */
  last_reply_at: z.number(),
  /** Up to 3 distinct most-recent repliers, most-recent first. Each is the SAME
   *  server-resolved {@link MessageAuthorDto} as `MessageDto.author` (#task:1791)
   *  so a hard-deleted replier keeps its snapshot identity; distinct by stable
   *  `author.id`, `unresolved` (id=null) repliers omitted. */
  repliers: z.array(MessageAuthorDtoSchema),
  /** Full distinct replier count (drives a "+N" overflow past the 3 shown). */
  replier_total: z.number().int().nonnegative(),
});
export type ReplyBadgeSummary = z.infer<typeof ReplyBadgeSummarySchema>;

/** One inline thread reply-PREVIEW row (#task:1746). A read-cluster's tail-3
 *  most-recent non-system replies are shown one-line each directly under the
 *  root message, so the reader gets the gist without opening the ThreadPanel.
 *  This is a READ-INERT projection: rendering it advances NO read cursor and
 *  changes no observed/unread/follow state (docs/thread-unread-v1.md §6) — it is
 *  strictly readability, not a read surface.
 *
 *  Eligibility mirrors `reply_counts_by_root` EXACTLY (`from_kind != 'system'`),
 *  so the previews are a subset of the same count and the two never diverge. */
export const ThreadReplyPreviewRowSchema = z.object({
  message_id: z.string(),
  /** The reply's conversation-global `MessageSeq`. PURELY the inter-message
   *  stream-ordering key used to select + order the tail-3 — NOT an
   *  edit/version freshness key. `seq` is assigned monotonically at insert and
   *  is thereafter immutable alongside `message_id` (there is no message
   *  edit/delete write path), so one `message_id` always carries one `seq`; a
   *  differing seq for the same id is malformed input, never a "newer edit".
   *  Payload freshness is handled by invalidate/refetch (see
   *  `reply_previews_by_root`), never by arbitrating seq. */
  seq: MessageSeqSchema,
  /** Reply author — the SAME server-authoritative {@link MessageAuthorDto} as
   *  `MessageDto.author` (#task:1791). A cluster's reply set is never system, so
   *  this is non-null; a hard-deleted replier still names itself via its
   *  send-time snapshot (`status='deleted'`) instead of collapsing to a directory
   *  miss. `unresolved` (legacy pre-snapshot deleted actor, `id=null`) is
   *  non-aggregating and the renderer shows a neutral "Unknown" placeholder. */
  author: MessageAuthorDtoSchema,
  /** A NORMALIZED MARKDOWN SOURCE EXCERPT of the reply — NOT semantically
   *  stripped plain text. Both producers route it through the shared
   *  {@link normalizeThreadPreviewExcerpt} SSOT: whitespace-compact (`\s+`→space)
   *  + trim + cap at 160 chars (the Activity `preview()` normalization; NO
   *  Markdown parser — there is no canonical MD→text helper and the read model
   *  must not grow one, since grammar-aware stripping is renderer-owned
   *  presentation). So literal `**`, backticks, `[x](y)` etc. MAY appear, and a
   *  token bisected by the 160-cap MAY be partial — this is accepted for v1.
   *  Both this and `ThreadSummary.root_preview` are Markdown SOURCE excerpts
   *  (not stripped), but they do NOT share normalization: `root_preview` is a
   *  raw slice, whereas this one is whitespace-compacted + trimmed for the
   *  single-line context. The renderer displays it single-line as ordinary text
   *  and owns the display ellipsis + mention sigils; its single-line width is
   *  far below 160, so the cap is a wire bound, not the display truncation. */
  preview: z.string(),
  /** Number of attachments on the reply — label metadata only, never the bytes. */
  attachment_count: z.number().int().nonnegative(),
  /** Canonical display LABEL of the FIRST attachment (`Image #N` / `File #N`),
   *  via the shared `resolveAttachmentLabels` SSOT, or null when the reply has
   *  none. This is the SAME label the message body and the agent stdin suffix
   *  reference, so the preview's "<label> +N" affordance stays in sync with the
   *  body — never the raw filename (which would decouple the preview from an
   *  inline "Image #1" reference). The renderer composes "+N" from
   *  `attachment_count`. No attachment content is read. */
  first_attachment_label: AttachmentLabelSchema.nullable(),
});
export type ThreadReplyPreviewRow = z.infer<typeof ThreadReplyPreviewRowSchema>;

/** Wire cap for a thread reply-preview excerpt (see {@link normalizeThreadPreviewExcerpt}).
 *  A bound on the projected SOURCE, not the renderer's single-line display width. */
export const THREAD_PREVIEW_MAX_CHARS = 160;

/** SSOT for the `ThreadReplyPreviewRow.preview` normalization (Option A, ruled
 *  #task:1746): whitespace-compact (`\s+`→space) + trim + cap at
 *  {@link THREAD_PREVIEW_MAX_CHARS}. This is a WIRE-PROJECTION rule, not visual
 *  parsing — it does NOT strip Markdown grammar (literal `**`, backticks,
 *  `[x](y)` survive; grammar-aware stripping is renderer-owned presentation).
 *
 *  Both the cold read-model producer (`MessageRepo.replyPreviewsByRoot`) and the
 *  live WS-merge producer (`threadReplyPreviewRowFromMessage`, #task:1745) MUST
 *  route the `preview` field through here, since it is required to be byte-identical
 *  across reload vs live merge — the same reason attachment labels share
 *  `resolveAttachmentLabels`. Does NOT govern `ThreadSummary.root_preview`, which
 *  is deliberately a RAW slice (no whitespace compaction). */
export function normalizeThreadPreviewExcerpt(content: string): string {
  return content.replace(/\s+/g, ' ').trim().slice(0, THREAD_PREVIEW_MAX_CHARS);
}

export const ConversationMessagesDtoSchema = z.object({
  messages: z.array(MessageDtoSchema),
  /** Reply-cluster badge data keyed by root message id. Values are the
   *  total non-system replies in that root's cluster, independent of the
   *  current pagination window. */
  reply_counts_by_root: z.record(z.string(), z.number().int().nonnegative()),
  /** Optional enriched reply-badge sidecar keyed by root message id
   *  (#task:1073, P1). Present only once the backend ships it; consumers
   *  must treat it as possibly-undefined and fall back to count-only. */
  reply_summaries_by_root: z.record(z.string(), ReplyBadgeSummarySchema).optional(),
  /** Optional inline thread reply-PREVIEW sidecar keyed by root message id
   *  (#task:1746). Each value is that root's tail-3 most-recent non-system
   *  replies, time-ASCending — a bounded 1..3 array (schema-enforced `.min(1)
   *  .max(3)`; the backend NEVER emits `[]`). Present only once the backend
   *  ships it; consumers treat it as possibly-undefined and fall back to the
   *  count-only chip. The FIELD is computed over the COMPLETE set of non-system
   *  top-level roots in the returned window, so its presence + a root's window
   *  membership disambiguate absence into THREE states:
   *   • WHOLE-FIELD absent (`undefined`) — this server does not project previews
   *     (old backend) / not projected on this response: UNKNOWN, never seedable.
   *   • field present, root IS a loaded eligible (non-system top-level) row, key
   *     MISSING — that root was authoritatively 0-eligible-reply at fetch time
   *     (0 replies ⇒ no preview key AND no reply chip, since the chip is gated on
   *     count>0; the first live reply opens both the chip and the tail in the
   *     same frame).
   *   • root NOT in the returned window — UNKNOWN (its tail was never computed).
   *  So a per-root missing key UNDER A PRESENT FIELD is an authoritative fetched-
   *  zero: the client MAY seed a fresh 1-row tail from a single live WS create
   *  for such a loaded eligible root (the common 0→1). It MUST NOT synthesize a
   *  tail when the WHOLE field is absent, or for a root outside the loaded
   *  eligible set. Eligibility mirrors `reply_counts_by_root`
   *  (non-system), so previews are a subset of that count. READ-INERT: rendering
   *  advances no cursor and changes no observed/unread/follow state. Freshness
   *  is a #task:1745 RENDERER concern — the backend adds no WS projection here:
   *  a WS create can patch the ordered, bounded set (append the newest, drop the
   *  oldest past 3) when the client already holds this root's tail; an
   *  edit/delete/hide/attachment change (dropping one of the 3 needs the 4th,
   *  which the client's 3 cannot supply) → invalidate + refetch this root. */
  reply_previews_by_root: z
    .record(z.string(), z.array(ThreadReplyPreviewRowSchema).min(1).max(3))
    .optional(),
  has_more_before: z.boolean(),
  /** Open-time snapshot of the opened stream's head (#task:1226): the
   *  `conversation_stream_heads.latest_seq` for the requested stream (mainline
   *  when no `thread_root_id`, else that reply-cluster), captured in the SAME
   *  read as this window — never a later/separate query, so a concurrent tail
   *  arriving after the window can't slip into the snapshot. The client echoes
   *  this as an `at_seq` position to `observe()` so the cursor advances to the
   *  open-time head, not to "current latest" — under v1 read-to-clear, opening a
   *  thread marks it read up to this head (the window, latest or
   *  around-first-unread, is a locate view, not a coverage guarantee); the
   *  invariant is only that a concurrent tail (seq > this value) is not
   *  swallowed. Window-independent. 0 for an empty stream. */
  stream_head_seq: MessageSeqSchema,
  /** True when at least one message exists with seq > max(messages.seq).
   *  For the default "latest" window this is always false; for an
   *  anchored window (`before_seq` / `after_seq` / `around_message_id`)
   *  it reflects the real DB state, so a bidirectional infinite query
   *  knows whether to issue a `getNextPageParam`. */
  has_more_after: z.boolean(),
});
export type ConversationMessagesDto = z.infer<typeof ConversationMessagesDtoSchema>;

export const ThreadFollowerStateDtoSchema = z.object({
  /** Canonical reply-cluster root; the request may name either the root
   *  message or any reply in that cluster. */
  thread_root_id: z.string(),
  /** Whether the current human viewer is subscribed to future delivery /
   *  notification for this thread. This is not a permission bit. */
  viewer_following: z.boolean(),
  /** Agent followers that are still visible members of the parent channel.
   *  Renderer uses this to filter channel-level run-state bubbles inside a
   *  thread panel. */
  follower_agent_ids: z.array(z.string()),
});
export type ThreadFollowerStateDto = z.infer<typeof ThreadFollowerStateDtoSchema>;

export const ThreadFollowMutationDtoSchema = z.object({
  thread_root_id: z.string(),
  changed: z.boolean(),
  viewer_following: z.boolean(),
});
export type ThreadFollowMutationDto = z.infer<typeof ThreadFollowMutationDtoSchema>;

export const SendMessageInputSchema = z
  .object({
    client_message_id: ClientMessageIdSchema.optional(),
    content: z.string(),
    /** Optional attachment ids previously returned by the upload endpoint.
     *  Server validates each id belongs to the same channel as the
     *  destination conversation. */
    attachment_ids: MessageAttachmentIdsInputSchema,
    /** Reply-cluster target message id. A main-line target opens a cluster;
     *  replying to an existing reply joins that reply's root. */
    reply_to_message_id: z.string().min(1).optional(),
  })
  .refine(hasMessagePayload, {
    message: 'message must include text or at least one attachment',
    path: ['content'],
  });
export type SendMessageInput = z.infer<typeof SendMessageInputSchema>;

/** Per-stream unread for one stream (mainline or a single reply-cluster),
 *  scored server-side against the viewer's own per-stream `conversation_reads`
 *  cursor (#task:1226). `unread_count` 0 ⟺ both first-unread fields null. */
export const StreamUnreadDtoSchema = z.object({
  unread_count: z.number().int().nonnegative(),
  /** First (lowest-seq) unread message in this stream — drives "jump to first
   *  unread". Both null when caught up (`unread_count === 0`). */
  first_unread_message_id: z.string().nullable(),
  first_unread_seq: MessageSeqSchema.nullable(),
});
export type StreamUnreadDto = z.infer<typeof StreamUnreadDtoSchema>;

/** Per-conversation per-viewer stream-unread read-model (#task:1226) — the
 *  SINGLE source of truth for thread / mainline "N new" + first-unread on the
 *  client. The reply chip and the Threads list both read this (keyed by root),
 *  so the same server-scored fact never lives in two caches that drift. Unread
 *  is server-scored (the client can't optimistically derive it), so this lives
 *  OUTSIDE the optimistically-patched message-window cache and is settled from
 *  REST after viewer-scoped `unread_updated` frames or local observe() success.
 *  `threads` is sparse: it carries ONLY roots with unread > 0 (a missing root
 *  means 0). */
export const ConversationStreamUnreadDtoSchema = z.object({
  /** Mainline (non-thread) unread for the viewer. */
  mainline: StreamUnreadDtoSchema,
  /** Per-reply-cluster unread keyed by root message id. Sparse — only roots
   *  with unread > 0 appear; absent root ⇒ 0 unread. FOLLOW-AGNOSTIC: this
   *  includes unfollowed threads (drives their in-list soft dot); it is NOT the
   *  header-dot authority. */
  threads: z.record(z.string(), StreamUnreadDtoSchema),
  /** Channel-header Threads-icon dot (#task:1774): true iff the viewer has any
   *  unread reply in a thread they FOLLOW (`followed ∩ unread`). FOLLOW-GATED and
   *  mainline-excluded — distinct from `threads` above, which is follow-agnostic.
   *  A boolean, not a count: the dot is a binary attention cue. Shares one server
   *  predicate with the `?filter=followed-unread` threads view, but the two are
   *  independent reads that converge via the same `unread_updated` invalidation —
   *  never derive list rows from this flag, nor overwrite it from an empty list. */
  followed_thread_unread: z.boolean(),
});
export type ConversationStreamUnreadDto = z.infer<typeof ConversationStreamUnreadDtoSchema>;

export const MessageUnreadPointerDtoSchema = z.object({
  conversation_id: z.string(),
  message_id: z.string(),
  seq: MessageSeqSchema,
  thread_root_id: z.string().nullable(),
});
export type MessageUnreadPointerDto = z.infer<typeof MessageUnreadPointerDtoSchema>;

export const MessageUnreadByConversationDtoSchema = z.object({
  by_conversation: z.record(z.string(), z.number()),
  latest_by_conversation: z.record(z.string(), MessageUnreadPointerDtoSchema),
  total: z.number(),
});
export type MessageUnreadByConversationDto = z.infer<typeof MessageUnreadByConversationDtoSchema>;

export const MessageUnreadSummaryDtoSchema = z.object({
  by_workspace: z.record(z.string(), z.number()),
});
export type MessageUnreadSummaryDto = z.infer<typeof MessageUnreadSummaryDtoSchema>;

/** The `unread_updated` MESSAGE-variant payload (viewer scope, #task:1283).
 *  Carries authoritative ABSOLUTE unread at each read-model boundary a badge
 *  consumer keys on, so the client idempotently patches the displayed badge
 *  caches instead of invalidate→whole-list refetch. All values are absolute
 *  (never deltas), from the same `ConversationReadRepo` REST serves.
 *  - `conversation_unread_count` → `by_conversation[conversation_id]`, favorites
 *                                   row `unread_count` (a favorite is a
 *                                   CONVERSATION since #task:1888)
 *  - `workspace_unread_count`    → `by_workspace[workspace_id]`, `total`
 *  - `latest_unread` (pointer, not a count) → `latest_by_conversation`;
 *    null ⇒ this conversation has no unread (cleared → delete the key).
 *  The two variants are discriminated SOLELY by `conversation_id` presence:
 *  the MESSAGE variant always carries it, the BADGE variant never does. The
 *  badge variant is not required to be empty (the mark-read route ships
 *  `{marked}`, and observe ships `dismissed_notification_ids` — see
 *  `UnreadUpdatedBadgePayload`); consumers must key on the discriminator, not on
 *  an empty body. See docs/projection-recovery-s1-unread-v1.md. */
export const UnreadUpdatedMessagePayloadSchema = z.object({
  conversation_id: z.string(),
  workspace_id: z.string(),
  conversation_unread_count: z.number(),
  workspace_unread_count: z.number(),
  latest_unread: MessageUnreadPointerDtoSchema.nullable(),
});
export type UnreadUpdatedMessagePayload = z.infer<typeof UnreadUpdatedMessagePayloadSchema>;

/** The `unread_updated` BADGE-variant payload (viewer scope). Discriminated
 *  from the message variant by the ABSENCE of `conversation_id`. Beyond the
 *  discriminator it MAY carry `dismissed_notification_ids` — the notification
 *  ids that just transitioned unread→read in this observe() batch. That field
 *  is consumed by the OS-notification-dismiss projection (#task:1431): the same
 *  "notification read" domain fact that refreshes the badge count also tells the
 *  desktop main process which OS notifications to `.close()`, so no read
 *  predicate is re-derived on the client. Other producers (the mark-read route)
 *  carry `marked` instead; both are optional and non-load-bearing for the badge
 *  count itself, which the client always refetches.
 *
 *  This BADGE variant is a REFETCH SIGNAL, not an absolute-count carrier. Source
 *  #3 (a directed notification going read via `POST /api/notifications/read`) is
 *  the case that relies on that: the Rust-migrated backend deliberately does NOT
 *  publish a standalone absolute `attention_badge_updated` for the mark-read
 *  path — an absolute count delivered out of commit order would let an earlier
 *  clear's stale value win under the client's last-write-wins badge apply — so
 *  the client re-reads the authoritative roll-up on this signal instead. (The
 *  general commit-order/versioning of the absolute `attention_badge_updated`
 *  frame across ALL producers is a separate, still-open concern.) This is a
 *  deliberate delivery divergence from the pre-migration TypeScript route, which
 *  shipped a standalone absolute badge per changed workspace; user-visible
 *  projection parity is preserved at the cost of one roll-up refetch per
 *  user-initiated clear. */
export interface UnreadUpdatedBadgePayload {
  dismissed_notification_ids?: string[];
  marked?: number;
}

const SearchOptionalMillisSchema = z.preprocess(
  (value) => (value === '' || value === undefined ? undefined : value),
  z.coerce.number().int().nonnegative().optional(),
);

// Stream scope for message search (#task:1334). `all` searches both the
// mainline and every thread of the matched conversation(s) — the historical
// behaviour. `mainline` / `thread` narrow to a single renderable stream of
// ONE conversation, backing the header inline find: the filter uses the same
// predicate as the message-window read-model, so a hit is always
// materializable in the panel that issued it ("搜到必可见").
export const SearchStreamSchema = z.enum(['all', 'mainline', 'thread']);
export type SearchStream = z.infer<typeof SearchStreamSchema>;

// Result ordering. `recency_desc` (default) is discovery order. `seq_asc`
// is conversation order for inline prev/next navigation — only meaningful
// within a single conversation, so it is gated on `conversation_id`.
export const SearchOrderSchema = z.enum(['recency_desc', 'seq_asc']);
export type SearchOrder = z.infer<typeof SearchOrderSchema>;

// The SENDER axis, defined once and spread into every read face that carries it
// (search + saved). It is ONE axis in two scoped shapes: a sender is a user or
// an agent, and the two id namespaces are disjoint, so the wire spells the kind
// structurally instead of tagging one `sender` string. "No sender filter" is
// therefore BOTH absent — never a `sender=''` sentinel the server would have to
// interpret.
//
// Both faces resolve a selection against the STABLE send-time author snapshot
// (`from_kind` + `author_id`), not the live FK — see `SavedMessagesQueryInputSchema`
// below for the full rule (#task:2059 / #task:2061). This fragment exists so the
// two faces cannot drift into two sender contracts: they shared the field pair
// and this refine by copy until #task:2061, and a copy is exactly what let their
// PREDICATES diverge for a release.
const SENDER_FILTER_FIELDS = {
  user_id: SearchOptionalStringSchema,
  agent_id: SearchOptionalStringSchema,
};

/** Single-select (@shiki, 2026-08-18): one sender at a time on both faces, so
 *  naming both is a client bug to refuse loudly, not a union to silently pick
 *  one arm from. */
const senderIsSingleSelect = (v: {
  user_id?: string | undefined;
  agent_id?: string | undefined;
}): boolean => !(v.user_id && v.agent_id);
const SENDER_SINGLE_SELECT_ISSUE: { message: string; path: (string | number)[] } = {
  message: 'user_id and agent_id cannot both be set',
  path: ['user_id'],
};

export const SearchQueryInputSchema = z
  .object({
    q: z.string().trim().max(200).optional().default(''),
    // Scope selection, not version negotiation. Inline search omits this.
    include_identities: z
      .union([z.boolean(), z.enum(['true', 'false']).transform((v) => v === 'true')])
      .optional(),
    stream: SearchStreamSchema.optional().default('all'),
    workspace_id: SearchOptionalStringSchema,
    conversation_id: SearchOptionalStringSchema,
    thread_root_id: SearchOptionalStringSchema,
    ...SENDER_FILTER_FIELDS,
    from: SearchOptionalMillisSchema,
    to: SearchOptionalMillisSchema,
    order: SearchOrderSchema.optional().default('recency_desc'),
    // Cap raised to 500 so inline find can return a complete single-stream
    // match set for prev/next; advanced search callers keep the 80 default.
    limit: z.coerce.number().int().positive().max(500).optional().default(80),
  })
  .refine(senderIsSingleSelect, SENDER_SINGLE_SELECT_ISSUE)
  // A stream only exists within a conversation; narrowing to one requires
  // naming which conversation.
  .refine((v) => v.stream === 'all' || !!v.conversation_id, {
    message: 'stream filter requires conversation_id',
    path: ['stream'],
  })
  // Thread scope needs the anchoring root message id.
  .refine((v) => v.stream !== 'thread' || !!v.thread_root_id, {
    message: 'stream=thread requires thread_root_id',
    path: ['thread_root_id'],
  })
  // seq is per-conversation; ordering by it across conversations is
  // meaningless, so seq_asc is gated on a single-conversation scope.
  .refine((v) => v.order !== 'seq_asc' || !!v.conversation_id, {
    message: 'order=seq_asc requires conversation_id',
    path: ['order'],
  });
export type SearchQueryInput = z.infer<typeof SearchQueryInputSchema>;

export const SearchWorkspaceRefSchema = z.object({
  id: z.string(),
  name: z.string(),
  /**
   * WHOSE container this is: the home agent of an agent space, `null` for a
   * regular workspace. The ownership arc itself — not a word derived from it.
   *
   * Search's consumer needs this bit for an IA-level decision (`groupResults`
   * splits an "Agent Spaces" group) and cannot compute it in scope, which is the
   * only reason a container fact travels on a result row at all: a ref carries a
   * fact ONLY when the reader's real question needs it and the DTO cannot
   * otherwise answer it (#task:2075).
   *
   * The published `kind` word (`z.enum(['regular','dm'])`) carried the same bit
   * on `origin/main` — it PREDATES this field, it was not introduced beside it.
   * A1 added `owner_agent_id` alongside the existing published `kind` so the
   * renderer half could switch reads without a lock-step change, and A2's
   * stop-publish step then deleted `kind` — a breaking removal of a field the
   * in-field renderer reads, one of this generation's desktop-epoch breaks
   * (#task:2075). Do not re-derive a
   * container word from this field.
   */
  owner_agent_id: z.string().nullable(),
});
export type SearchWorkspaceRef = z.infer<typeof SearchWorkspaceRefSchema>;

/** Fields of a search conversation ref that do not depend on placement. */
const searchConversationRefBase = {
  id: z.string(),
  title: z.string(),
  roster_policy: ConversationRosterPolicySchema,
  channel_id: z.string(),
  closed_at: z.number().nullable(),
  created_at: z.number(),
};

// #task:2053-C — the conversation ref is a DISCRIMINATED UNION on
// `placement_kind`, the SINGLE authority for "where is this conversation". The
// workspace ref lives INSIDE the workspace arm — workspace is a property of the
// conversation's PLACEMENT, not of the search result — so the discriminator and
// the field it governs sit at the SAME level: `conv.placement_kind ===
// 'workspace'` narrows the type, and the `direct_pair` arm has no `workspace`
// field to read at all. The "workspace present iff workspace-placed" invariant
// is therefore a TYPE FACT the compiler enforces on the consumer, not a
// cross-layer superRefine that only holds at runtime (which would leave the
// consumer free to branch on `workspace === null` — the same field-absence-as-
// discriminator failure mode as the `resolveNavWorkspaceId` finding).
//
// Only the two placements search's producer whitelist admits appear here, which
// is today the whole placement alphabet. The union is still the fail-closed
// guard, and for a FUTURE placement rather than a retired one: a row the SQL
// whitelist mis-projected fails to PARSE instead of arriving with its `workspace`
// field silently absent (#task:2053-B②) — both guards, same direction, and
// adding a placement must add an arm here or stay out of search.
export const SearchConversationRefSchema = z.discriminatedUnion('placement_kind', [
  z.object({
    ...searchConversationRefBase,
    placement_kind: z.literal('workspace'),
    workspace: SearchWorkspaceRefSchema,
  }),
  z.object({
    ...searchConversationRefBase,
    placement_kind: z.literal('direct_pair'),
  }),
]);
export type SearchConversationRef = z.infer<typeof SearchConversationRefSchema>;

export const SearchResultDtoSchema = z.discriminatedUnion('type', [
  z.object({
    type: z.literal('workspace'),
    // The workspace-DISCOVERY arm: the matched entity IS a workspace, so this
    // is the workspace itself, not a placement container. Unaffected by
    // #task:2053 — it always carries a real workspace.
    workspace: SearchWorkspaceRefSchema.extend({
      description: z.string().nullable(),
      archived_at: z.number().nullable(),
      created_at: z.number(),
      updated_at: z.number(),
    }),
  }),
  z.object({
    type: z.literal('conversation'),
    // No result-level `workspace`: the container now lives inside the
    // placement-discriminated `conversation` ref (#task:2053-C). This arm's
    // `direct_pair` case is structurally unreached today — a pair's stored
    // title is `''` (`DIRECT_PAIR_STORED_TITLE`) and discovery requires a
    // non-empty query. Only direct channel hits carry archive state;
    // the shared placement ref used by message context stays unchanged.
    conversation: SearchConversationRefSchema.and(z.object({ archived_at: z.number().nullable() })),
  }),
  z.object({
    type: z.literal('message'),
    // The container is `conversation.<workspace arm>.workspace`; this is the
    // arm where the `direct_pair` case is actually REACHED (a message in a
    // workspace-less human↔agent DM).
    conversation: SearchConversationRefSchema,
    message: MessageDtoSchema,
  }),
]);
export type SearchResultDto = z.infer<typeof SearchResultDtoSchema>;

/** Public identity data is complete: rendering must not join a directory cache. */
const searchIdentityFields = {
  id: z.string().min(1),
  name: z.string().min(1),
  display_name: z.string().min(1),
  avatar_url: z.string().nullable(),
};
const searchHumanSchema = z.object({
  ...searchIdentityFields,
  kind: z.literal('human'),
  runtime: z.null(),
});
const searchAgentSchema = z.object({
  ...searchIdentityFields,
  kind: z.literal('agent'),
  runtime: RuntimeKindSchema,
});
export const SearchIdentitySchema = z.discriminatedUnion('kind', [
  searchHumanSchema,
  searchAgentSchema,
]);
export type SearchIdentity = z.infer<typeof SearchIdentitySchema>;

const identityCategory = <T extends z.ZodTypeAny>(row: T) =>
  z
    .object({
      results: z.array(row).max(20),
      total: z.number().int().nonnegative(),
      truncated: z.boolean(),
    })
    .superRefine((value, ctx) => {
      if (
        value.results.length !== Math.min(value.total, 20) ||
        value.truncated !== value.total > 20
      ) {
        ctx.addIssue({ code: z.ZodIssueCode.custom, message: 'invalid identity category count' });
      }
    });
/** Each kind has its own 20-row budget, unrelated to content limit/total.
 * Counts are after ACL, active materialization and agent scope admission.
 * Order: lowercase display_name, lowercase name, then id (Unicode code-point order).
 * workspace/conversation/sender/time coordinates suppress identity search;
 * empty text or a lone @ does not enumerate identities. One leading @ is removed.
 */
export const SearchIdentitiesSchema = z
  .object({
    status: z.enum(['searched', 'filtered', 'empty_query']),
    humans: identityCategory(searchHumanSchema),
    agents: identityCategory(searchAgentSchema),
  })
  .superRefine((value, ctx) => {
    if (value.status !== 'searched' && (value.humans.total !== 0 || value.agents.total !== 0)) {
      ctx.addIssue({
        code: z.ZodIssueCode.custom,
        message: 'inactive identity search must be empty',
      });
    }
  });
export type SearchIdentities = z.infer<typeof SearchIdentitiesSchema>;

export const SearchDtoSchema = z.object({
  query: SearchQueryInputSchema,
  results: z.array(SearchResultDtoSchema),
  identities: SearchIdentitiesSchema.optional(),
  // Total message matches for the query BEFORE the result cap, and whether
  // the returned list was truncated to `limit`. Drives the inline find
  // "2 / 7" (exact) vs "2 / 200+" (truncated) counter and tells the
  // renderer prev/next only spans the returned set. Named-entity
  // (workspace/conversation) results are not counted here — this is the
  // message-match total. (#task:1334)
  total: z.number(),
  truncated: z.boolean(),
});
export type SearchDto = z.infer<typeof SearchDtoSchema>;
/** A request for identities must not silently turn a missing field into no matches. */
export const SearchWithIdentitiesDtoSchema = SearchDtoSchema.extend({
  identities: SearchIdentitiesSchema,
});

//
// One global flat list — no per-channel grouping, no tags. Each entry
// joins back to the source message + its place (workspace channel or DM) so the
// Saved view can render content + jump back to the conversation.

/** Where a saved message lives, as a discriminated union on the canonical
 *  human-conversation-place `kind` (`workspace_channel` / `direct_pair`, the
 *  same vocabulary Search reuses — NOT a private `place_kind`). The routing
 *  coordinate `conversation_id` is folded into each arm, so a `SavedPlace` is
 *  the message's complete location with no parallel top-level coordinate.
 *
 *  - workspace_channel arm: `conversation_id` plus the workspace board namespace
 *    (`workspace_id` alone — a container WORD sat beside it until #task:2075's
 *    stop-publish step, and no consumer of this place ever asked which kind of
 *    container it was) and the one embedded mutable name, `channel_name`. The workspace NAME is deliberately
 *    NOT here (F1, mirrors the shipped `TaskPlace` workspace arm): a workspace's
 *    name is its aggregate's mutable field, so the renderer joins it live from the
 *    `['workspaces']` directory (kept fresh by `viewer_workspaces_changed`,
 *    §4-B) rather than freezing a per-read-model snapshot. `channel_name` stays
 *    embedded, aligned with the shipped #2041 `TaskPlace`, and is a LIVE
 *    projection: `['saved']` MUST subscribe `workspace_channels_changed` (§4-A)
 *    and INVALIDATE only — never apply the payload as a snapshot. (The full
 *    coordinate-only endgame — join `channel_name` too — is deferred to #2045:
 *    the client channel-name join authority already exists
 *    (`useChannelsAcrossWorkspaces`), so it is a three-consumer convergence, not
 *    new infrastructure. The name-join model holds ONLY while task/saved
 *    visibility stays participation-only; widening to "workspace member may read"
 *    would make the join miss names that an embed would not — that is the real
 *    falsifier for this axis. See `docs/placement-contract.md`.)
 *  - direct_pair arm: ONLY `conversation_id`. No title (a DM stores none) and no
 *    peer name/avatar — the per-viewer `dm:@other` label and the peer identity
 *    are resolved live via the DirectPair carrier (contract §4-C), never frozen
 *    into a saved row.
 *
 *  Display/namespace ONLY — access control NEVER reads it (the ACL always
 *  recomputes from `conversation_id` back to the canonical participation
 *  authority; contract §4a: a Place DTO is not an authorization source). */
export const SavedPlaceSchema = z.discriminatedUnion('kind', [
  z.object({
    kind: z.literal('workspace_channel'),
    conversation_id: z.string(),
    workspace_id: z.string(),
    /** The channel's display title (a channel has one; a DM does not). A LIVE
     *  projection: `['saved']` subscribes `workspace_channels_changed` (§4-A)
     *  and invalidates on it. The workspace NAME is deliberately absent —
     *  joined live from `['workspaces']` (F1, mirrors `TaskPlace`). */
    channel_name: z.string(),
  }),
  z.object({
    kind: z.literal('direct_pair'),
    conversation_id: z.string(),
  }),
]);
export type SavedPlace = z.infer<typeof SavedPlaceSchema>;

export const SavedMessageDtoSchema = z.object({
  id: z.string(),
  message: MessageDtoSchema,
  /** The saved message's place: the routing coordinate
   *  (`place.conversation_id`) folded with the placement-specific display
   *  coordinates (see SavedPlaceSchema). Narrow on `place.kind` — workspace
   *  fields are structurally ABSENT on the pair arm. This is THE conversation
   *  coordinate; there is no separate top-level `conversation_id`/`channel_id`.
   *
   *  `place.conversation_id` is the SOLE conversation coordinate here, and it
   *  names the PARENT CHANNEL — it equals `message.conversation_id` by
   *  construction (the row is written from it: `saved-message-repo.ts`). A
   *  thread is a reply-cluster INSIDE that same conversation, and
   *  `message.thread_root_id` is a MESSAGE id, not a conversation coordinate:
   *  there is no "thread conversation" to address. So a saved reply carries its
   *  channel's id here, exactly like a saved top-level message, and no consumer
   *  may read any `conversation_id` as a thread coordinate — attributing or
   *  filtering a row "by thread" from this field silently means "by channel". */
  place: SavedPlaceSchema,
  saved_at: z.number(),
});
export type SavedMessageDto = z.infer<typeof SavedMessageDtoSchema>;

/** Query for `GET /api/saved` (#task:1258). Mirrors
 *  `ArchivedChannelsQueryInputSchema`: the Saved view filters SERVER-SIDE so the
 *  filters act over the whole saved history, not the `LIMIT`-capped client
 *  window (a client-side date/workspace filter would silently drop saved
 *  messages older than the newest N). `workspace_id` narrows to one workspace
 *  (omitted = all workspaces the caller can see); `since` is `saved_at >= since`
 *  (epoch ms) — the SAVE-time window, not message created/updated time, matching
 *  the list's `saved_at DESC, id DESC` total order. All are applied before
 *  `ORDER BY … LIMIT`.
 *
 *  `user_id` / `agent_id` narrow by SENDER (#task:2059). They are the shared
 *  `SENDER_FILTER_FIELDS` axis above, spread — not copied — so this face and
 *  search cannot grow two different sender contracts (they did diverge once,
 *  which is what #task:2061 fixed).
 *
 *  BOTH faces match on the stable send-time snapshot (`from_kind` +
 *  `author_id`), never on the live FK (`from_user_id` / `from_agent_id`), which
 *  `ON DELETE SET NULL` clears when the actor is hard-deleted: an FK predicate
 *  silently drops every message from a since-deleted sender. This is the author
 *  identity rule already used for distinct-replier grouping (#task:1791). For an
 *  actor that still exists the two keys are row-equivalent, so the snapshot key
 *  costs nothing and only differs where it matters. A row with neither FK nor
 *  snapshot (`author_id IS NULL` → `unresolved`) has no nameable identity and is
 *  matched by NO sender — never attributed to the viewer. A sender id is never
 *  validated for existence — an unknown id simply matches nothing, so this
 *  filter is not an existence oracle for user/agent ids. */
export const SavedMessagesQueryInputSchema = z
  .object({
    workspace_id: z.preprocess(
      (v) => (typeof v === 'string' && v.trim() === '' ? undefined : v),
      z.string().optional(),
    ),
    since: z.preprocess(
      (v) => (v === '' || v === undefined ? undefined : v),
      z.coerce.number().int().nonnegative().optional(),
    ),
    ...SENDER_FILTER_FIELDS,
  })
  .refine(senderIsSingleSelect, SENDER_SINGLE_SELECT_ISSUE);
export type SavedMessagesQueryInput = z.infer<typeof SavedMessagesQueryInputSchema>;

//
// The plain `@name` form: a DISPLAY alias, not an address.
//
// #task:2000 split the two axes. Delivery — who a message addresses — is
// decided exclusively by canonical reference destinations
// (`reference.ts` + `reference-markdown.ts`); this lexical form never
// participates in it, in any position. What survives here is the grammar the
// renderer uses to draw a chip for a bare `@name`, and the composer uses to
// offer rewriting one into a canonical reference before send.
//
// It is a PERMANENT alias, not a migration bridge: a message body is immutable
// after insert (no write path anywhere sets `content`), so history will never
// become canonical, and dropping the alias would mean silently degrading every
// message ever written.
//
// The boundary check rejects emails (`me@x`) and paths (`a/@b`), and the
// alpha-first requirement rejects issue refs like `@123`. The inner name
// grammar is `PARTICIPANT_HANDLE_PATTERN`, shared with the stored schema and
// with the handle segment of a reference destination; this pattern adds only
// the boundary needed to survive prose. Exported as a SOURCE fragment (one
// capture group, no flags) so each consumer owns its own regex object and no
// caller can inherit another's `lastIndex`.
export const PLAIN_MENTION_PATTERN = `(?<![\\w/])@(${PARTICIPANT_HANDLE_PATTERN})`;
