/**
 * System-message subtype vocabulary and defensive metadata readers.
 * Rust owns message production; these schemas decode persisted/wire metadata.
 * The Desktop registry uses Record<SystemSubtype, ...> for exhaustive dispatch.
 * Unknown or retired subtypes degrade to generic narration.
 */
import { z } from 'zod';
import { TaskAssigneeDtoSchema } from './api/task.js';
import { TaskStatusSchema } from './db/schema.js';

/** Live subtypes — every entry must have a renderer in the registry. */
export const SYSTEM_SUBTYPES = [
  'onboarding',
  'channel_context',
  'child_context',
  'thread_anchor',
  'mention_cc',
  'schedule_fire',
  'session_cleared',
  'conversation_member_added',
  'conversation_member_removed',
  'rename',
  'task_create',
  'task_claim',
  'task_assign',
  'task_unclaim',
  'task_status',
  'task_edit',
  'message_answer',
] as const;
export type SystemSubtype = (typeof SYSTEM_SUBTYPES)[number];

/** Task lifecycle subtypes accepted by the metadata readers. */
export const TASK_SUBTYPES = [
  'task_create',
  'task_claim',
  'task_assign',
  'task_unclaim',
  'task_status',
  'task_edit',
] as const;
export type TaskSubtype = (typeof TASK_SUBTYPES)[number];

/** Subtype resolution result. `current` rows are renderer registry's
 *  responsibility; `unknown` rows fall through to generic narration and are
 *  observable as drift / malformed / migrated-away history. */
export type SubtypeResolution = { kind: 'current'; subtype: SystemSubtype } | { kind: 'unknown' };

/**
 * Resolve a raw subtype string (or null/undefined) to one of two states.
 * Renderers consume this — they switch on `resolution.kind` first, then
 * dispatch on `resolution.subtype` only in the `current` branch.
 */
export function resolveSubtype(raw: string | null | undefined): SubtypeResolution {
  if (raw == null) return { kind: 'unknown' };
  if ((SYSTEM_SUBTYPES as readonly string[]).includes(raw)) {
    return { kind: 'current', subtype: raw as SystemSubtype };
  }
  return { kind: 'unknown' };
}

// ── Per-subtype metadata schemas ─────────────────────────────────────
//
// Producers and renderers stay loose at the message-row boundary
// (`metadata: Record<string, unknown>` in the message DTO), but each
// subtype with structured fields owns a zod schema below so producers
// can lock the shape on insert and renderers can defensively parse on
// read. Renderers do NOT throw on missing fields — they fall back to a
// degraded display (typically: drop the structured pieces and render
// `message.content` as narration). This is the "legacy task row" path
// from the PR-2 design discussion.

/** Actor on a task system event — derived from the `TaskActor` that
 *  triggered the mutation. `id` is `null` only for legacy user actors
 *  whose `userId` was unknown at insert time; new producers set
 *  `id = 'local'` (or whatever the principal carries). */
export const TaskSystemActorSchema = z.discriminatedUnion('kind', [
  z.object({
    kind: z.literal('user'),
    id: z.string().nullable(),
    name: z.string(),
  }),
  z.object({
    kind: z.literal('agent'),
    id: z.string(),
    name: z.string(),
  }),
]);
export type TaskSystemActor = z.infer<typeof TaskSystemActorSchema>;

/** Actor + member snapshot fields shared by the member added / removed
 *  system messages — both events carry the identical participant pair, so
 *  the only thing that differs is the `subtype` discriminator. */
const conversationMemberChangeFields = {
  actor_kind: z.enum(['user', 'agent', 'system']),
  actor_id: z.string(),
  actor_name: z.string(),
  actor_display_name: z.string().nullable(),
  member_kind: z.enum(['human', 'agent']),
  member_id: z.string(),
  member_name: z.string(),
  member_display_name: z.string().nullable(),
} as const;

export const ConversationMemberAddedMetadataSchema = z.object({
  subtype: z.literal('conversation_member_added'),
  ...conversationMemberChangeFields,
});
export type ConversationMemberAddedMetadata = z.infer<typeof ConversationMemberAddedMetadataSchema>;

export const ConversationMemberRemovedMetadataSchema = z.object({
  subtype: z.literal('conversation_member_removed'),
  ...conversationMemberChangeFields,
});
export type ConversationMemberRemovedMetadata = z.infer<
  typeof ConversationMemberRemovedMetadataSchema
>;

/** Union of the two member-change events. The frontend renders both with
 *  one row component (icon + verb + preposition keyed off `subtype`), so a
 *  single reader that keeps the discriminator is what it consumes. */
export const ConversationMemberChangeMetadataSchema = z.discriminatedUnion('subtype', [
  ConversationMemberAddedMetadataSchema,
  ConversationMemberRemovedMetadataSchema,
]);
export type ConversationMemberChangeMetadata = z.infer<
  typeof ConversationMemberChangeMetadataSchema
>;

export function readConversationMemberAddedMetadata(
  metadata: Record<string, unknown> | null | undefined,
): ConversationMemberAddedMetadata | null {
  if (!metadata) return null;
  const parsed = ConversationMemberAddedMetadataSchema.safeParse(metadata);
  return parsed.success ? parsed.data : null;
}

export function readConversationMemberChangeMetadata(
  metadata: Record<string, unknown> | null | undefined,
): ConversationMemberChangeMetadata | null {
  if (!metadata) return null;
  const parsed = ConversationMemberChangeMetadataSchema.safeParse(metadata);
  return parsed.success ? parsed.data : null;
}

/**
 * Metadata shape for `task_create` / `task_claim` / `task_assign` / `task_unclaim` /
 * `task_status` / `task_edit` system messages. Two rules hold for EVERY event:
 * `subtype` names the ACTION, and `task_status` is the status snapshot AFTER
 * the mutation. **No branch lets you derive the prior status from `subtype`** —
 * the same subtype means different things per `actor.kind`, and even the agent's
 * workflow verbs accept several prior states:
 *
 *  - `subtype = task_create`: genesis event; `task_status = 'todo'`.
 *  - `subtype = task_claim`, `actor.kind = 'agent'`: the agent's self-claim, a
 *    workflow verb. It accepts an unassigned (or already its own) non-terminal
 *    task: a `todo` task is bumped to `in_progress`, while `in_progress` /
 *    `in_review` are left as they are. Read the field, don't assume the prior
 *    was `todo`.
 *  - `subtype = task_assign`: an agent or human made `task_assignee` the owner.
 *    An OWNERSHIP write: `task_status` is the unchanged current status.
 *  - `subtype = task_claim`, `actor.kind = 'user'`: legacy (pre-`task_assign`)
 *    human assign; older servers also started a `todo` task in the same write.
 *  - `subtype = task_unclaim`, `actor.kind = 'agent'`: the agent's self-release,
 *    a workflow verb — it returns the task to the pool, so
 *    `task_status = 'todo'` and `task_assignee = null`. The prior status was any
 *    non-terminal state the agent was assigned to; it is NOT carried.
 *  - `subtype = task_unclaim`, `actor.kind = 'user'`: a human unassign — an
 *    OWNERSHIP write. `task_assignee = null` and `task_status` is the UNCHANGED
 *    current status.
 *  - `subtype = task_status`: arbitrary transition; the prior state is
 *    NOT in metadata — needs back-reference to the previous task system
 *    message if a future view wants to render a `before → after` diff.
 *  - `subtype = task_edit`: a title/content DEFINITION change; `task_status`
 *    is the CURRENT status (an edit never moves status), so it is NOT a
 *    transition. The metadata deliberately carries NO body/content — a reader
 *    (or a woken assignee) pulls the current definition via `task detail`.
 *    Fired ONLY on a real canonical change; a same-value save emits nothing.
 *
 * Assignment and unassignment touch ownership only; starting or rewinding the
 * workflow stays exclusive to the assignee's own claim / unclaim. Legacy events
 * predating the actor field have no `actor.kind` to branch on — degrade, don't
 * guess.
 *
 * `task_id` (v60+) is the canonical task-aggregate id for per-task event
 * lookup. The legacy `task_message_id` (pre-v60 events) held the old
 * body/anchor message id from the "task IS a message" model — same value for
 * every mutation event on a task; new producers omit it and read `task_id`.
 * For migrated events the two are equal (a migrated task's id IS its old
 * anchor id).
 */
export const TaskSystemMetadataSchema = z.object({
  subtype: z.enum(TASK_SUBTYPES),
  task_number: z.number(),
  /** Status snapshot AFTER this mutation. The event type is `subtype`, not this
   *  field, and no subtype implies a prior status — see the schema docstring. */
  task_status: TaskStatusSchema,
  /** The task identity (`tasks.id`) this event belongs to — the canonical key
   *  for opening the task from its system event. New producers always write it.
   *  For migrated events it equals the legacy `task_message_id` (a migrated
   *  task's id IS its old anchor message id), backfilled by the v60 migration. */
  task_id: z.string(),
  /** @deprecated Legacy anchor-message id from the "task IS a message" model.
   *  Present on pre-v60 events; new producers omit it (a task is no longer a
   *  message). Read `task_id` instead. */
  task_message_id: z.string().optional(),
  task_title: z.string(),
  task_assignee: TaskAssigneeDtoSchema.nullable(),
  actor: TaskSystemActorSchema,
  /** True ONLY on a `task_create` event that was filed from a DIFFERENT
   *  conversation than the one it landed in (an agent's cross-conversation
   *  create via `--conversation-id`; destination may be any conversation,
   *  including a DM). This bit is the SINGLE SOURCE OF TRUTH for the
   *  destination-visible wording: the agent-facing narration (`content`) and
   *  the desktop enriched row BOTH derive "filed … from another conversation"
   *  vs "created …" from it — neither reads the other's face. HARD PRIVACY
   *  INVARIANT: it carries NO origin conversation id/title/ref, so a
   *  destination member cannot locate, search, or contact the origin (the
   *  origin→destination mapping lives only in the backend restricted audit
   *  log). Absent = false: legacy events and every same-conversation create
   *  render "created". Only ever set for subtype `task_create`. */
  cross_conversation: z.boolean().optional(),
});
export type TaskSystemMetadata = z.infer<typeof TaskSystemMetadataSchema>;

/** An ownership write by someone other than the owner: `task_assign`, plus the
 *  legacy human `task_claim` and any human `task_unclaim`. */
export function isTaskAssignmentEvent(meta: TaskSystemMetadata): boolean {
  if (meta.subtype === 'task_assign') return true;
  return (
    (meta.subtype === 'task_claim' || meta.subtype === 'task_unclaim') && meta.actor.kind === 'user'
  );
}

/** TS alias to flag the `task_status` field as the after-state snapshot.
 *  Read sites can write `const status: TaskStatusAfterMutation = m.task_status`
 *  to make the semantic explicit at the call site. */
export type TaskStatusAfterMutation = TaskSystemMetadata['task_status'];

/**
 * Defensive parse of a system-message metadata object into a fully
 * enriched `TaskSystemMetadata`. Returns `null` if any required field is
 * missing or malformed — renderers should chain into
 * `readTaskSystemMetadataPartial` for legacy rows inserted before
 * PR-2 added the enrichment, and only drop to raw `message.content`
 * narration when even the partial schema fails.
 */
export function readTaskSystemMetadata(
  metadata: Record<string, unknown> | null | undefined,
): TaskSystemMetadata | null {
  if (!metadata) return null;
  const parsed = TaskSystemMetadataSchema.safeParse(metadata);
  return parsed.success ? parsed.data : null;
}

/**
 * Partial schema for legacy task system metadata. Pre-PR-2 producers
 * only wrote `subtype` + `task_number` + `task_status`, with no actor /
 * task_title / task_assignee / task_message_id enrichment. Renderers
 * use this as a midway fallback: if the full schema fails but the
 * partial one passes, draw a degraded row (chip + status pill, no
 * actor pill / verb / title) instead of dropping all the way to
 * narration. There are ~1646 such rows in production DBs as of PR-2.
 */
export const TaskSystemMetadataPartialSchema = z.object({
  subtype: z.enum(TASK_SUBTYPES),
  task_number: z.number(),
  task_status: TaskStatusSchema,
});
export type TaskSystemMetadataPartial = z.infer<typeof TaskSystemMetadataPartialSchema>;

/** Defensive partial parse — returns `null` if even the minimal
 *  `(subtype, task_number, task_status)` triple is unparseable. */
export function readTaskSystemMetadataPartial(
  metadata: Record<string, unknown> | null | undefined,
): TaskSystemMetadataPartial | null {
  if (!metadata) return null;
  const parsed = TaskSystemMetadataPartialSchema.safeParse(metadata);
  return parsed.success ? parsed.data : null;
}

// ── session_cleared metadata ─────────────────────────────────────────

/**
 * Metadata shape for `session_cleared` system messages — emitted when
 * a user clears a (conversation, agent)'s CLI session via the
 * `/clear-session` route. Renderers draw a compact audit row off
 * these fields without re-parsing `message.content`.
 *
 *  - `agent_id` / `agent_name`: which agent's session was reset.
 *  - `actor_user_id`: who triggered it; `null` for legacy v1 rows
 *    where principal info wasn't stored.
 *  - `previous_cli_session_id_prefix`: short id (~8 chars) of the
 *    session that was cleared, surfaced as a `prev <id>` chip;
 *    `null` if the agent had no prior session (cold path).
 *  - `was_running`: true if the agent process was live at clear time.
 *    Renderer surfaces "was running" / "was idle" so the audit row
 *    captures whether this clear interrupted active work.
 */
export const SessionClearedMetadataSchema = z.object({
  subtype: z.literal('session_cleared'),
  agent_id: z.string(),
  agent_name: z.string(),
  actor_user_id: z.string().nullable(),
  previous_cli_session_id_prefix: z.string().nullable(),
  was_running: z.boolean(),
});
export type SessionClearedMetadata = z.infer<typeof SessionClearedMetadataSchema>;

/** Defensive parse for session_cleared metadata. Returns `null` for
 *  legacy rows whose metadata predates this schema; renderer falls
 *  back to a generic audit row reading `message.content`. */
export function readSessionClearedMetadata(
  metadata: Record<string, unknown> | null | undefined,
): SessionClearedMetadata | null {
  if (!metadata) return null;
  const parsed = SessionClearedMetadataSchema.safeParse(metadata);
  return parsed.success ? parsed.data : null;
}

// ── schedule_fire metadata ───────────────────────────────────────────

/**
 * Metadata shape for `schedule_fire` system messages — emitted by
 * the scheduler when a wake fires for an agent. Renderers read the
 * structured fields to draw a flat banner with header + status pills
 * + the original wake body, without re-parsing `message.content`.
 *
 *  - `schedule_id`: stable id of the underlying schedule row.
 *  - `title`: short UI label for the wake. Optional — legacy rows
 *    fall back to the generic "Scheduled wake" header.
 *  - `spec_kind`: `'once' | 'interval' | 'cron'` — drives the header
 *    spec pill grouping.
 *  - `spec_summary`: pre-rendered human label
 *    (e.g. `"cron *\/5 9-18 * * 1-5 (Asia/Shanghai)"`, `"interval 60s"`,
 *    `"once"`). Cron summaries embed the timezone because the same
 *    expression resolves to different wall-clock times across zones.
 *    Optional — legacy rows without it fall back to `spec_kind` alone.
 *  - `body`: the user's reminder text (`schedule.message`) without
 *    the producer's "Schedule fired (kind: …)" / "Next fire at …" /
 *    "If nothing to do, reply 'noted, no action'" scaffolding. The
 *    scaffolding is for the agent's stdin; humans reading the banner
 *    only need the reminder. Optional — legacy rows fall back to
 *    rendering full `message.content`.
 *  - `actual_fire_at`: epoch ms when the wake actually fired.
 *  - `planned_fire_at`: epoch ms the wake was originally scheduled
 *    for. Difference vs. actual surfaces as the "planned <Δ ago/late>"
 *    pill. Set on every fire, including one-shot — the producer always
 *    writes `dto.next_fire_at` here. Schema keeps it nullable to absorb
 *    pre-PR-4 rows that lacked the field; renderers should suppress
 *    the pill only when the value is `null`.
 *  - `missed_count`: how many planned fires were skipped before this
 *    one (catch-up semantic). `0` is common; non-zero highlights as
 *    a warning pill.
 *  - `next_fire_at`: epoch ms of the next scheduled fire. `null` for
 *    one-shot or expired repeating schedules — pill is suppressed.
 */
export const ScheduleFireMetadataSchema = z.object({
  subtype: z.literal('schedule_fire'),
  schedule_id: z.string(),
  title: z.string().optional(),
  spec_kind: z.enum(['once', 'interval', 'cron']),
  spec_summary: z.string().optional(),
  body: z.string().optional(),
  actual_fire_at: z.number(),
  planned_fire_at: z.number().nullable(),
  missed_count: z.number(),
  next_fire_at: z.number().nullable(),
});
export type ScheduleFireMetadata = z.infer<typeof ScheduleFireMetadataSchema>;

/** Defensive parse for schedule_fire metadata. Returns `null` for
 *  legacy rows or malformed metadata; renderer falls back to a
 *  muted banner that reads `message.content` as a single-line body. */
export function readScheduleFireMetadata(
  metadata: Record<string, unknown> | null | undefined,
): ScheduleFireMetadata | null {
  if (!metadata) return null;
  const parsed = ScheduleFireMetadataSchema.safeParse(metadata);
  return parsed.success ? parsed.data : null;
}

// ── context-card series (PR-5) ───────────────────────────────────────
//
// `thread_anchor`, `child_context`, and `channel_context` form a
// unified context-card series. Their producer-visible difference is
// the relationship to a parent conversation:
//
//  - `thread_anchor` ("Quoted"):     the new thread quotes a single
//    message from the parent channel — body = the quoted text.
//  - `child_context` ("Inherited"):  a sub-thread inherits a free-form
//    summary the spawner wrote about the parent's prior context.
//  - `channel_context` ("Spawned-from"): a new top-level channel
//    inherits a seed body authored by the spawning agent in another
//    conversation.
//
// All three share the same render shape (left accent line, kind tag,
// source chip, collapsible body) but with different accent colors and
// tag labels. Each carries enough metadata that the renderer never
// has to re-parse `message.content` to identify the source — the
// content remains intact for agent stdin (which reads imperative
// prose), while metadata feeds the human banner.

/** Allowed `from_kind` values for the message a `thread_anchor`
 *  quotes. `system` is included because edge cases — a thread spawned
 *  off a system message — are technically reachable. */
export const QuotedFromKindSchema = z.enum(['user', 'agent', 'system']);
export type QuotedFromKind = z.infer<typeof QuotedFromKindSchema>;

/**
 * Metadata shape for `thread_anchor` system messages — emitted as the
 * first seed when a thread is created off a specific parent message
 * (right-click "Reply as thread" / "Discuss in thread").
 *
 *  - `quoted_message_id` / `quoted_conversation_id`: pointer at the
 *    original message + parent conversation; lets the renderer offer
 *    "jump to original" without re-querying.
 *  - `quoted_channel_name`: pre-rendered parent label (workspace name
 *    or channel title) for the source chip.
 *  - `quoted_content`: full text of the original message, preserved
 *    inline so the body stays visible after the parent is archived
 *    or the original message is later edited.
 *  - `quoted_from_kind` / `quoted_from_agent_id` / `quoted_from_user_id`
 *    / `quoted_agent_name`: identity of the original author. Agent
 *    rows carry both id+name; user rows carry id (legacy rows may
 *    have null id).
 *  - `quoted_created_at`: epoch ms of the original message — surfaced
 *    as the "<X ago>" right-aligned timestamp.
 */
export const ThreadAnchorMetadataSchema = z.object({
  subtype: z.literal('thread_anchor'),
  quoted_message_id: z.string(),
  quoted_conversation_id: z.string(),
  quoted_channel_name: z.string(),
  quoted_content: z.string(),
  quoted_from_kind: QuotedFromKindSchema,
  quoted_from_agent_id: z.string().nullable(),
  quoted_from_user_id: z.string().nullable(),
  quoted_agent_name: z.string().nullable(),
  quoted_created_at: z.number(),
});
export type ThreadAnchorMetadata = z.infer<typeof ThreadAnchorMetadataSchema>;

/** Defensive parse for thread_anchor metadata. Returns `null` for
 *  legacy rows whose metadata predates this schema; renderer falls
 *  back to muted narration over `message.content`. */
export function readThreadAnchorMetadata(
  metadata: Record<string, unknown> | null | undefined,
): ThreadAnchorMetadata | null {
  if (!metadata) return null;
  const parsed = ThreadAnchorMetadataSchema.safeParse(metadata);
  return parsed.success ? parsed.data : null;
}

/**
 * Metadata shape for `child_context` system messages — emitted as the
 * first seed when an agent spawns a sub-thread with a free-form
 * `contextSummary`. `message.content` keeps the imperative prose
 * (`Inherited context from parent channel '<title>': <summary>`)
 * because the spawner agent's stdin reads it; renderers consume the
 * structured triple instead.
 *
 *  - `parent_conversation_id`: pointer at the parent channel for
 *    "jump to original" affordances.
 *  - `parent_title`: pre-rendered parent label for the source chip.
 *  - `summary`: the agent-authored context blurb, without the
 *    `Inherited context from parent channel '...': ` scaffolding.
 */
export const ChildContextMetadataSchema = z.object({
  subtype: z.literal('child_context'),
  parent_conversation_id: z.string(),
  parent_title: z.string(),
  summary: z.string(),
});
export type ChildContextMetadata = z.infer<typeof ChildContextMetadataSchema>;

/** Defensive parse for child_context metadata. */
export function readChildContextMetadata(
  metadata: Record<string, unknown> | null | undefined,
): ChildContextMetadata | null {
  if (!metadata) return null;
  const parsed = ChildContextMetadataSchema.safeParse(metadata);
  return parsed.success ? parsed.data : null;
}

/**
 * Metadata shape for `channel_context` system messages — emitted as the
 * first seed when an agent calls `silverhand channel create` and the
 * service inserts the agent-authored `systemMessageBody` as the new
 * channel's initial context. Renderer surfaces it as a Spawned-from
 * card with a chip pointing back at the source conversation.
 *
 *  - `source_conversation_id`: pointer at the conversation the agent
 *    spawned this channel from.
 *  - `source_conversation_title`: pre-rendered source label for the
 *    chip; legacy rows may omit it (renderer falls back to "another
 *    conversation").
 *  - `created_by_agent_id` / `created_by_agent_name`: identity of the
 *    spawning agent. `created_by_agent_name` is new in PR-5 — legacy
 *    rows fall back to "an agent" in the source chip.
 *
 * Note: the seed body lives in `message.content`. We do NOT mirror it
 * into metadata — there's no producer-side scaffolding to strip (the
 * agent wrote the body to stdout-cleanly), and duplicating it would
 * just bloat the row.
 */
export const ChannelContextMetadataSchema = z.object({
  subtype: z.literal('channel_context'),
  source_conversation_id: z.string(),
  source_conversation_title: z.string().optional(),
  created_by_agent_id: z.string(),
  created_by_agent_name: z.string().optional(),
});
export type ChannelContextMetadata = z.infer<typeof ChannelContextMetadataSchema>;

/** Defensive parse for channel_context metadata. */
export function readChannelContextMetadata(
  metadata: Record<string, unknown> | null | undefined,
): ChannelContextMetadata | null {
  if (!metadata) return null;
  const parsed = ChannelContextMetadataSchema.safeParse(metadata);
  return parsed.success ? parsed.data : null;
}

// ── rename metadata (PR-6) ───────────────────────────────────────────

/**
 * Metadata shape for `rename` system messages — emitted when a
 * renamable entity's display name changes. Two scopes share this one
 * subtype:
 *   - `scope: 'workspace'` — emitted into every open channel of a
 *     workspace when the workspace name changes (names are plain
 *     handles, rendered `#old → #new`).
 *   - `scope: 'conversation'` — emitted into a single channel when its
 *     own title changes (titles are freeform — may contain spaces /
 *     CJK — so the renderer quotes them rather than using a `#` sigil).
 * The renderer uses the structured names + scope to draw a compact
 * audit row without re-parsing the prose.
 *
 * ONE subtype, no address-change sibling (#task:2016). A channel's title IS its
 * address, so `scope: 'conversation'` announces both at once — there is no second
 * kind of channel rename for a reader to have to tell this one apart from. The
 * old title is released by the same statement and nothing remembers it, so a
 * reference to it stops resolving and MAY later resolve to a different channel
 * that takes the title; that is why the message this metadata accompanies tells
 * members to update stored references.
 *
 *  - `old_name` / `new_name`: the name before and after the change.
 *  - `scope`: which kind of entity was renamed. Defaults to
 *    `'workspace'` so legacy rows (written before channel rename
 *    existed, when the only producer was workspace rename) parse
 *    correctly without a stored field.
 *
 * Note: there is no `actor_user_id` field. The route is user-principal
 * only (single-user v1) so the actor is implicit. If multi-user
 * arrives, add an actor field at the same time as the auth model.
 */
export const RenameMetadataSchema = z.object({
  subtype: z.literal('rename'),
  old_name: z.string(),
  new_name: z.string(),
  scope: z.enum(['workspace', 'conversation']).default('workspace'),
});
export type RenameMetadata = z.infer<typeof RenameMetadataSchema>;

/** Defensive parse for rename metadata. Returns `null` for legacy
 *  rows that predate this schema; renderer falls back to generic
 *  italic narration over `message.content`. */
export function readRenameMetadata(
  metadata: Record<string, unknown> | null | undefined,
): RenameMetadata | null {
  if (!metadata) return null;
  const parsed = RenameMetadataSchema.safeParse(metadata);
  return parsed.success ? parsed.data : null;
}

// ── message_answer metadata ──────────────────────────────────────────

/**
 * Metadata of the `message_answer` system event posted in the question's stream
 * on every answer and re-answer (#task:3143). The labels are snapshots taken at
 * answer time, so the line stays readable however the question later reads.
 *
 *  - `answer_to`: the question message id (click target).
 *  - `question_author`: who asked; the event mentions them.
 *  - `answerer`: who answered.
 *  - `options`: the picked options, in the question's order.
 *  - `other`: free text given instead of / beside the options, else null.
 *  - `changed`: true when this replaced an earlier answer.
 */
export const MessageAnswerMetadataSchema = z.object({
  subtype: z.literal('message_answer'),
  answer_to: z.string(),
  question_author: z.object({
    kind: z.enum(['user', 'agent']),
    id: z.string(),
    name: z.string(),
  }),
  answerer: z.object({
    kind: z.enum(['user', 'agent']),
    id: z.string(),
    name: z.string(),
  }),
  options: z.array(z.object({ id: z.string(), label: z.string() })),
  other: z.string().nullable(),
  changed: z.boolean(),
});
export type MessageAnswerMetadata = z.infer<typeof MessageAnswerMetadataSchema>;

export function readMessageAnswerMetadata(
  metadata: Record<string, unknown> | null | undefined,
): MessageAnswerMetadata | null {
  if (!metadata) return null;
  const parsed = MessageAnswerMetadataSchema.safeParse(metadata);
  return parsed.success ? parsed.data : null;
}
