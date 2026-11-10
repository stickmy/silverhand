// Attention bounded context — read-side (CQRS) projection of the human's
// "queue waiting on me" (#task:1227). Bounded context: Attention (internal);
// user-facing surface/feature name: Activity. Re-exported via api/index.ts →
// @silverhand/shared. Canonical: docs/activity-lifecycle-v1.md.
//
// Attention owns exactly TWO write-side aggregates, both per-viewer user
// DECISIONS (建表准入测试, §4.2):
//   1. the per-(viewer, channel) attention mode — the channel
//      `所有消息 / 仅提及 / 静音` control (#task:1870);
//   2. the per-(viewer, workspace) mute SUPPRESSOR — a boolean that silences a
//      whole workspace without erasing any channel's own choice.
// Everything else Attention surfaces (positional unread / directed mentions /
// tasks) is a projection, never an Attention-owned copy.
import { z } from 'zod';
import { DirectPairCounterpartSchema } from './conversation.js';
import { MessageAuthorDtoSchema } from './message-author.js';

/**
 * Channel attention mode (§6 / §13.3) — ONE mutually-exclusive choice per
 * (viewer, channel), so it is a single three-value enum rather than an ambient
 * flag plus a separate `muted` boolean: the illegal combination
 * (`muted && ambient ON`) must be inexpressible in the type, not merely
 * unrendered. The user wording is 所有消息 / 仅提及 / 静音 (the renderer maps the
 * label, never the internal word):
 *   - `all`      = ambient ON  → the channel's mainline ambient unread enters
 *                  the Activity list and counts toward the loud badge.
 *   - `mentions` = ambient OFF → ambient unread stays an in-place hint only
 *                  (quiet), and only directed claims (@me) count toward the
 *                  badge. Directed claims still enter Activity unconditionally.
 *   - `mute`     = ambient OFF and the in-place positional hint is suppressed
 *                  too (silent). Directed claims are NOT suppressed: @me still
 *                  enters Activity, still toasts, still notifies. "Mute"
 *                  silences the AMBIENT channel, never the person addressing you.
 *
 * The three values are three loudness levels of ONE badge, not three
 * eligibilities: `enter()`'s ambient test stays binary (`effective === 'all'`),
 * and `mentions` / `mute` differ only in how loudly the residue is presented.
 */
export const CHANNEL_ATTENTION_MODES = ['all', 'mentions', 'mute'] as const;
export type ChannelAttentionMode = (typeof CHANNEL_ATTENTION_MODES)[number];

/** Channel default = ambient ON (§6: channel → All messages unless opted down).
 *  Absence of a stored preference resolves to this. Single source so the read
 *  path and `enter()` (②c) never disagree on the default. */
export const DEFAULT_CHANNEL_ATTENTION_MODE: ChannelAttentionMode = 'all';

/** Workspace default = NOT muted. Absence of a stored row resolves to this;
 *  single source so the read path, `enter()` and the badge projection never
 *  disagree. */
export const DEFAULT_WORKSPACE_ATTENTION_MUTED = false;

/** The viewer's STORED mute state for one REGULAR workspace (GET / PUT
 *  `/api/workspaces/:id/attention-preference`). Not called "effective": in this
 *  context `effective` means "a channel tier with the workspace suppressor
 *  folded in", and nothing sits above the workspace scope to fold. Deliberately
 *  a BOOLEAN, not a
 *  fourth tier and not an overridable default: the workspace scope is a
 *  SUPPRESSOR that sits above the channel tier, so it neither needs the channel
 *  enum's third value nor may it rewrite a channel's own stored choice. Turning
 *  the workspace mute off restores every channel's tier exactly as it was ⇒ no
 *  migration, no backfill, no inherit axis. */
export const WorkspaceAttentionPreferenceDtoSchema = z.object({
  workspace_id: z.string(),
  muted: z.boolean(),
});
export type WorkspaceAttentionPreferenceDto = z.infer<typeof WorkspaceAttentionPreferenceDtoSchema>;

/** PUT body for the viewer's workspace-level mute suppressor. */
export const SetWorkspaceAttentionPreferenceInputSchema = z.object({
  muted: z.boolean(),
});
export type SetWorkspaceAttentionPreferenceInput = z.infer<
  typeof SetWorkspaceAttentionPreferenceInputSchema
>;

/** The channel's OWN STORED ambient tier (GET / PUT
 *  `/api/conversations/:id/attention-preference`). `mode` is always resolved —
 *  the default is applied server-side when no row exists, so the client never
 *  branches on "unset" — but it is deliberately NOT the effective tier: this
 *  surface neither reads nor folds the workspace suppressor, because it is the
 *  face the human WRITES, and what they wrote must come back unchanged even
 *  while the workspace is muted. {@link ChannelAttentionModeRowDtoSchema} is the
 *  read face that carries both `mode` and `effective_mode`. */
export const ChannelAttentionPreferenceDtoSchema = z.object({
  conversation_id: z.string(),
  mode: z.enum(CHANNEL_ATTENTION_MODES),
});
export type ChannelAttentionPreferenceDto = z.infer<typeof ChannelAttentionPreferenceDtoSchema>;

/** PUT body for setting the viewer's attention mode for a channel. Together with
 *  the workspace suppressor PUT these are the only surfaces that write attention
 *  state (§5.2); neither ever touches delivery/follow. */
export const SetChannelAttentionPreferenceInputSchema = z.object({
  mode: z.enum(CHANNEL_ATTENTION_MODES),
});
export type SetChannelAttentionPreferenceInput = z.infer<
  typeof SetChannelAttentionPreferenceInputSchema
>;

// ─────────────────── Badge projection (#task:1870 B) ───────────────────
//
// The three tiers ride a SEPARATE presentation projection with its own
// archived/liveness guard. The raw positional 口径 (`unread_updated`'s
// `workspace_unread_count`, the sidebar roll-up, `FavoriteRowDto.unread_count`,
// the notification panel's `by_workspace`) is deliberately untouched — a tiered
// badge is an additional face, not a redefinition of unread. Anything that means
// "how many messages are unread" keeps its old answer; only the new
// `badge_count` means "how loudly should this workspace shout".
//
// badge_count(workspace) =
//     Σ raw positional unread over `all`-tier conversations   (loud residue)
//   + Σ unread `mention_self` over `mentions`/`mute`-tier ones (directed residue)
// The tiers PARTITION the viewer's conversations, so an `all`-tier channel never
// double-counts an @me: §8's positional count already includes it.

/** `GET /api/attention/badges` — the viewer's app-level badge roll-up.
 *  `by_workspace` omits workspaces whose badge is 0 (absent ≡ 0), matching the
 *  notification roll-up's shape. `total = direct_pairs + Σ by_workspace` is the
 *  whole app badge (Dock, menu bar). */
export const AttentionBadgeRollupDtoSchema = z.object({
  by_workspace: z.record(z.string(), z.number()),
  /** Unread across the viewer's direct pairs, which have no workspace. A pair is
   *  always `all` tier (its whole positional unread counts); a pair whose
   *  counterpart is deleted is skipped, as the sidebar hides it. Its live carrier
   *  is `direct_pairs_changed`, a refetch signal for this roll-up. Absent ≡ 0
   *  (an older server, whose `total` then excludes DMs). */
  direct_pairs: z.number().int().nonnegative().optional(),
  total: z.number(),
  /** The workspaces the viewer has MUTED (#task:1877), so the rail can mark a
   *  muted row without a second request. Absent ≡ not muted, exactly like
   *  `by_workspace`, and scoped to the workspaces the viewer can currently see —
   *  the mute preference deliberately outlives a workspace removal (a re-join
   *  restores it), so this list is not "every stored preference".
   *
   *  A SIBLING of `by_workspace`, not a field inside it: that map is keyed by
   *  "has a non-zero badge", and a muted workspace is usually at 0 and therefore
   *  ABSENT — there would be no entry to hang the flag on, and forcing zeros in
   *  would destroy the "keys ≡ which workspaces have something waiting" reading
   *  (which since #task:1939 is read by the sidebar's wake gate, not painted on
   *  the workspace row).
   *
   *  Not a second authority either: the suppressor row remains the only place
   *  mute is decided (`PUT /api/workspaces/:id/attention-preference`); this is
   *  that state projected onto the roll-up, which is why a workspace-level
   *  `attention_pref_changed` must also invalidate this read. */
  muted_workspace_ids: z.array(z.string()),
});
export type AttentionBadgeRollupDto = z.infer<typeof AttentionBadgeRollupDtoSchema>;

/** One channel's resolved tier inside `GET /api/attention/modes`. `mode` is the
 *  channel's OWN stored (or defaulted) choice; `effective_mode` folds in the
 *  workspace suppressor — both are carried so the UI can render "muted by the
 *  workspace" without re-deriving the rule, and so flipping the workspace mute
 *  off restores a visible per-channel choice.
 *
 *  `directed_unread_count` is what makes a quiet/silent row EXPLAINABLE: a
 *  `mute` channel's directed residue is exactly what makes THAT ROW loud
 *  (`channelRowBadge`) and, since #task:1939, what surfaces it out of a collapsed
 *  workspace — without it a muted channel contributing directed residue to the
 *  workspace aggregate would have no visible cause. The aggregate itself is no
 *  longer displayed on the workspace row (docs/activity-lifecycle-v1.md §8
 *  「呈现」); the row is where the explanation lands. */
export const ChannelAttentionModeRowDtoSchema = z.object({
  conversation_id: z.string(),
  mode: z.enum(CHANNEL_ATTENTION_MODES),
  effective_mode: z.enum(CHANNEL_ATTENTION_MODES),
  directed_unread_count: z.number().int().nonnegative(),
});
export type ChannelAttentionModeRowDto = z.infer<typeof ChannelAttentionModeRowDtoSchema>;

/** `GET /api/attention/modes?workspace_id=…` — one batch read of every channel
 *  tier in ONE workspace, so the sidebar renders N rows without N requests.
 *  Covers exactly the viewer's member channels in that workspace. */
export const WorkspaceAttentionModesDtoSchema = z.object({
  workspace_id: z.string(),
  /** The workspace suppressor's own stored value (default applied server-side).
   *  Carried so the UI can label the rows below as "muted by the workspace". */
  muted: z.boolean(),
  channels: z.array(ChannelAttentionModeRowDtoSchema),
});
export type WorkspaceAttentionModesDto = z.infer<typeof WorkspaceAttentionModesDtoSchema>;

/** Viewer-scoped `attention_badge_updated` payload — an ABSOLUTE per-workspace
 *  badge value, patched into the roll-up cache exactly like
 *  `unread_updated`'s `workspace_unread_count` (never a delta). It is emitted
 *  from ALL THREE authoritative update sources, because the badge is a function
 *  of all three and the consumer never refetches it on its own schedule (no
 *  polling, `refetchOnWindowFocus: false`; the global 5s `staleTime` only decides
 *  freshness at mount / invalidation, and this query stays mounted for the app's
 *  lifetime):
 *    1. positional unread changing (send / read / observe),
 *    2. a pref write on either aggregate (channel tier, workspace suppressor),
 *    3. a directed notification being inserted or marked read.
 *  Missing any one source does not open a transient window — it FREEZES the
 *  badge at a wrong value until an unrelated event happens to arrive. Source (3)
 *  is why this is its own event rather than a field on `unread_updated`: the
 *  mention row is inserted AFTER that frame is built, so a piggybacked count
 *  would be computed before the fact it must reflect existed. */
export const AttentionBadgeUpdatedPayloadSchema = z.object({
  workspace_id: z.string(),
  badge_count: z.number().int().nonnegative(),
});
export type AttentionBadgeUpdatedPayload = z.infer<typeof AttentionBadgeUpdatedPayloadSchema>;

/** Viewer-scoped `attention_pref_changed` payload — IDENTITY only, deliberately
 *  no resolved value: the tier read face is the single authority, and a value
 *  here would be a second one (a consumer that patches from the event can then
 *  disagree with `GET /api/attention/modes`). Consumers re-read the affected
 *  tier face and the Activity list. `conversation_id` is NULL when the change was
 *  the workspace suppressor (which re-resolves every channel in the workspace). */
export const AttentionPrefChangedPayloadSchema = z.object({
  workspace_id: z.string(),
  conversation_id: z.string().nullable(),
});
export type AttentionPrefChangedPayload = z.infer<typeof AttentionPrefChangedPayloadSchema>;

// ───────────────────────────── Activity list (②c) ─────────────────────────────
//
// `GET /api/activity` returns the viewer's Activity list — the projection of
// `listActivity(viewer, scope)` (§9.2). Pure read side: every item is computed
// from an existing source (per-stream cursor read-model / `notifications`),
// NEVER from an Attention-owned copy. The server returns the
// list already `enter()`-filtered (§6), per-(viewer, message) de-duplicated (§7),
// and salience-sorted (§13.2: directed tier first, most-recent-first within tier).

/**
 * Salience tier (§5.2) — derived, never stored. `directed` items are visually
 * stronger and sort above `ambient`. DM unread is positional by mechanism but
 * `directed`-grade by salience (§5.1: "永远进, directed-grade").
 */
export const ACTIVITY_SALIENCES = ['directed', 'ambient'] as const;
export type ActivitySalience = (typeof ACTIVITY_SALIENCES)[number];

/**
 * The claim class of an Activity item — drives the icon and the eliminate route
 * (§5.3). `mention` / `agent_failed` clear via `notifications.read_at`; `unread`
 * clears via the stream cursor (`observe`).
 *   - `mention`      = a `mention_self` notification (directed @me).
 *   - `agent_failed` = an `agent_start_failed` notification (the viewer's agent).
 *   - `unread`       = a positional stream's ambient/DM unread aggregate.
 *
 * NOTE — there is no `task` kind in v1. A standing `in_review && member`
 * projection of task state mirrored the task panel as a permanent,
 * non-eliminable item; tasks belong in the task panel. A task source re-enters
 * Activity only when it is event-shaped & notification-backed
 * (`task_needs_review`), read-cleared like a mention — see
 * docs/activity-lifecycle-v1.md §5.1 (v2).
 */
export const ACTIVITY_ITEM_KINDS = ['mention', 'agent_failed', 'unread'] as const;
export type ActivityItemKind = (typeof ACTIVITY_ITEM_KINDS)[number];

/**
 * Author of an item's `preview` message — the SAME server-authoritative
 * {@link MessageAuthorDto} as `MessageDto.author` (#task:1791). The GLOBAL
 * cross-workspace list resolves it from the anchor message in `listActivity` so
 * it never client-stitches a sender from a per-workspace member cache (which it
 * does not have), AND a hard-deleted sender still names itself via its
 * send-time snapshot (`status='deleted'`) instead of collapsing to a NULL byline
 * once `ON DELETE SET NULL` clears the FK. `avatar_url` is the same relative,
 * cache-busted path baked into `UserDto`/`AgentDto`; `status` drives the
 * renderer's active/deleted/unresolved presentation. `kind` picks the avatar
 * component (UserAvatar vs AgentAvatar).
 *
 * This is additive display data ONLY: it carries NO source_ref and is NOT part
 * of the eliminate route (§5.3). NULL when `preview` has no message author
 * (`agent_failed`, whose preview comes from the notification payload; and any
 * `system` anchor — system is not an actor).
 */
/**
 * WHERE an Activity item happened — the row-1 location identity, as ONE
 * discriminated union rather than a set of independently-defaultable top-level
 * fields (#task:2115).
 *
 * The list is GLOBAL, so it spans both conversation placements, and the two have
 * disjoint identity vocabularies: a workspace channel is identified by its
 * workspace + its own title, while a `direct_pair` has NEITHER coordinate — its
 * stored title is the empty string (`DIRECT_PAIR_STORED_TITLE`) and it has no
 * workspace_id at all. Carrying that as `workspace_id: string | null` +
 * `title: string` would make the illegal states (a DM with a workspace, a
 * channel without one, a pair with a synthesized title) representable, and the
 * renderer would have to re-derive the placement from which fields happen to be
 * NULL. Here the arm IS the answer, and `location.kind` is the renderer's single
 * dispatch point.
 *
 * The DM arm carries the `counterpart` VALUE OBJECT, deliberately not a
 * server-rendered display title: the counterpart's name, its deleted-endpoint
 * wording and its avatar variant are UI copy, owned by the renderer's
 * `presentCounterpart` SSOT (the same one the DM list and pane use). A synthesized
 * `title` here would fork a second label vocabulary that drifts from it. This is
 * ALSO why the field is unconditional on the item's `kind`: an `agent_failed` row
 * on a DM still needs a name for row 1 (it keeps its own failure glyph in the
 * identity slot, but the name line is still the counterpart's) — so `counterpart`
 * hangs off the location, which always knows it, never off the item kind.
 *
 * NOTE this union is NOT `ConversationPlacementKind`: `workspace_channel` is the
 * Activity-side word for "workspace-placed ⇒ a channel" (#task:1373), a
 * conclusion this read model may draw because every workspace conversation it can
 * surface is a channel. Keying identity on PLACEMENT rather than on roster is the
 * #task:2076/#task:2077 judgment, unchanged: a fixed-roster channel inside an
 * agent home is still a channel and still renders as one.
 */
export const ActivityLocationSchema = z.union([
  z.object({
    kind: z.literal('workspace_channel'),
    workspace_id: z.string(),
    /** ALWAYS the conversation title — never a message excerpt. */
    title: z.string(),
  }),
  z.object({
    kind: z.literal('direct_pair'),
    /** The OTHER endpoint of the pair, as seen by this viewer — the same
     *  {@link DirectPairCounterpartSchema} the DM list carries, with current (not
     *  snapshotted) name/avatar and a `deleted` arm the renderer words itself. */
    counterpart: DirectPairCounterpartSchema,
  }),
]);
export type ActivityLocation = z.infer<typeof ActivityLocationSchema>;

/** One row in the Activity list. Heterogeneous by `kind`: directed
 *  single-event items (`mention` / `agent_failed`) anchor on one message; an
 *  `unread` item is a per-STREAM aggregate (§7.3) carrying a count and anchored
 *  at its first-unread message.
 *
 *  §5.3 answerability — every item's eliminate route (source_ref-out) is
 *  reconstructable from EXPLICIT fields here; `id` is a render list key ONLY and
 *  MUST NOT be parsed for source routing:
 *    - `mention` / `agent_failed` → set `notifications.read_at` by
 *      `notification_id`.
 *    - `unread`                   → advance the stream cursor (`observe`) keyed
 *      by `conversation_id` + `thread_root_id`. */
export const ActivityItemDtoSchema = z.object({
  /** Stable synthesized list key for the client (NOT a stored row id, and NOT a
   *  source_ref — never parse it; use the explicit source fields below). */
  id: z.string(),
  kind: z.enum(ACTIVITY_ITEM_KINDS),
  salience: z.enum(ACTIVITY_SALIENCES),
  /** Source routing (the §5.3 source_ref). */
  conversation_id: z.string(),
  /** Which stream: NULL = mainline / channel-level; non-NULL = that reply
   *  cluster. Carried for `mention` items too, so routing reaches the thread. */
  thread_root_id: z.string().nullable(),
  /** The anchor message the item points at. NULL only for `agent_failed` (no
   *  anchor message — pure `read_at` clearing). `mention` / `unread` carry the
   *  anchor message id. */
  message_id: z.string().nullable(),
  /** The notification this item eliminates through (`notifications.read_at`).
   *  Set for `mention` / `agent_failed`; NULL for `unread` (which clears via the
   *  stream cursor). This is the source_ref-out for directed notification-backed
   *  claims — the renderer marks read BY this id, never by parsing `id`. */
  notification_id: z.string().nullable(),
  // NO conversation-classification field rides this DTO beyond
  // {@link ActivityLocationSchema}'s own discriminant — in particular no roster
  // bit (#task:2076). Row-1 identity branches on PLACEMENT, which `location`
  // already answers by construction; roster's live role for Activity is
  // SERVER-SIDE salience (a fixed-roster stream's ambient unread is
  // directed-grade, §5.1), a capability question answered in
  // `read-model/activity.ts` off the SQL row that never needed to cross the wire.
  // `conversation_roster_policy` used to be here, and its docblock instructed the
  // renderer to derive a "Channel / DM" label from it — which is exactly how an
  // agent home's DM/Onboarding channel got drawn as a private line. The keying
  // rule itself lives on `ConversationPlacementKindSchema` (db/schema).
  /** Row-1 location identity + the source's workspace coordinate, if it has one.
   *  A discriminated union because the two placements identify themselves by
   *  different things and neither's fields are meaningful for the other. */
  location: ActivityLocationSchema,

  /** For a thread item (`thread_root_id != null`): a one-line excerpt of the
   *  thread's ROOT message, rendered above the reply `preview` so the row reads
   *  root → reply. NULL for non-thread items (mainline unread, channel mention,
   *  agent_failed) — there is no distinct root to show. */
  thread_root_preview: z.string().nullable(),
  /** Short preview of the anchor (message excerpt / failure summary). For a
   *  thread item this is the REPLY content (paired with `thread_root_preview`). */
  preview: z.string().nullable(),
  /** Author of `preview` (§row-3 `<sender>: <content>`), server-resolved from
   *  the anchor message — the SAME {@link MessageAuthorDto} as `MessageDto.author`
   *  (a hard-deleted sender keeps its snapshot identity + `deleted` status).
   *  NULL when `preview` has no message author (`agent_failed` / system anchor).
   *  Carries no source_ref — purely for rendering. */
  preview_sender: MessageAuthorDtoSchema.nullable(),
  /** Recency used for the within-tier sort (epoch ms). */
  timestamp: z.number(),
  /** `unread` items: count of ambient unread in the stream, already net of any
   *  directed claims in it (§8 `positional \ directed`). NULL for directed
   *  single-event items. */
  unread_count: z.number().int().nonnegative().nullable(),
});
export type ActivityItemDto = z.infer<typeof ActivityItemDtoSchema>;

/** `GET /api/activity` response. `items` is the fully-resolved list (filtered,
 *  deduped, sorted) — the client renders it as-is and never re-scores. */
export const ListActivityDtoSchema = z.object({
  items: z.array(ActivityItemDtoSchema),
});
export type ListActivityDto = z.infer<typeof ListActivityDtoSchema>;

// ───────────────────────────── observe() (#task:1231) ─────────────────────────────
//
// `POST /api/attention/observe` — the SINGLE eliminate primitive. Every
// read-cleared Activity item collapses into one use case `observe(source_ref[])`
// that delegates the write back to the source aggregate (per-stream cursor /
// `notifications.read_at`); Attention owns NO read state of its own. The legacy
// channel mark-read route was retired into this primitive (#task:1232).
// The schema below is the public wire contract; the backend use case owns the
// transactional routing and overlap semantics.
//
// Disambiguation: this `observe` (attention eliminate) is UNRELATED to the
// thread-delivery `observe` kind (backfill / non-waking). The `/api/attention/`
// prefix + this bounded context disambiguate the two.

/**
 * Where to advance a stream's cursor to — a TYPED variant, never collapsed to
 * "server takes current head" implicitly.
 *   - `at_seq`      = a client-supplied, already-resolved seq (thread open-time
 *     snapshot head / visible-live seq). An AUTO path (ThreadPanel open-time /
 *     visible-live) MUST use this — pinning the observed seq is what stops the
 *     §9.10 tail-swallow.
 *   - `stream_head` = server-resolved current head of the stream. For an
 *     EXPLICIT user clear (channel/thread open + ambient hover-✓ + Mark all
 *     read), which knowingly accepts swallowing the concurrent tail. Legal for
 *     ANY stream, thread included — the auto-vs-explicit discipline is a CALLER
 *     property, not a thread-shape ban (a followed-thread `unread` Activity item
 *     must be hover-✓ / Mark-all clearable).
 */
export const StreamPositionSchema = z.discriminatedUnion('kind', [
  z.object({ kind: z.literal('at_seq'), seq: z.number().int().nonnegative() }),
  z.object({ kind: z.literal('stream_head') }),
]);
export type StreamPosition = z.infer<typeof StreamPositionSchema>;

/**
 * One eliminate target = one source_ref (answerability: source_ref-in =
 * source_ref-out). A `stream` source advances the per-stream cursor (and folds
 * in the overlap double-clear, §3); a `notification` source clears that one
 * notification's `read_at` by id (directed dismiss — never reverse-advances a
 * cursor). Field names are snake_case so the renderer builds a source_ref
 * straight from the `ActivityItemDto` fields it already holds.
 */
export const ObserveSourceSchema = z.discriminatedUnion('kind', [
  z.object({
    kind: z.literal('stream'),
    conversation_id: z.string().min(1),
    /** NULL = mainline; a root message id = that reply-cluster. */
    thread_root_id: z.string().min(1).nullable(),
    position: StreamPositionSchema,
  }),
  z.object({
    kind: z.literal('notification'),
    notification_id: z.string().min(1),
  }),
]);
export type ObserveSource = z.infer<typeof ObserveSourceSchema>;

/** `POST /api/attention/observe` body. One source = a single eliminate; many =
 *  a Mark-all batch (each source resolved/written independently, but the whole
 *  batch is one transaction — any illegal source fails the WHOLE request, no
 *  partial clear). The cap is a runaway guard, not a product limit. */
export const ObserveInputSchema = z.object({
  sources: z.array(ObserveSourceSchema).min(1).max(1000),
});
export type ObserveInput = z.infer<typeof ObserveInputSchema>;

/** `POST /api/attention/observe` response. `changed` = at least one cursor
 *  advanced or one `read_at` was set (drives whether a single coalesced
 *  `unread_updated` fired). */
export const ObserveDtoSchema = z.object({
  changed: z.boolean(),
});
export type ObserveDto = z.infer<typeof ObserveDtoSchema>;
