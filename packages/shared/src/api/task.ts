// AUTO-SPLIT from the former monolithic api/dto.ts (#task:963).
// Bounded context: task. Re-exported via api/index.ts → @silverhand/shared.
import { z } from 'zod';
import { TaskAssigneeTypeSchema, TaskStatusSchema, type TaskStatus } from '../db/schema.js';
import { AgentCapabilitySchema } from './workspace.js';
import { CanonicalPositiveIntegerQuerySchema, PositiveJsSafeIntegerSchema } from './wire-number.js';

// ── Task title / content input spec (single source of truth) ──────────
// Both create and edit — across all three entry points (agent CLI, human
// HTTP, human UI) — validate title/content through these exact schemas and
// constants. The backend schema + service are authoritative; the renderer
// only soft-counts. Never define a second title/content bound elsewhere.

/** Max length of a task title, counted in UTF-16 code units (the unit Zod
 *  `.max()` measures — NOT graphemes; an emoji or astral char counts as 2). */
export const TASK_TITLE_MAX_LEN = 200;
/** Max length of a task's content/body. The task aggregate owns its `content`
 *  column directly; this is the task-domain bound. */
export const TASK_CONTENT_MAX_LEN = 10000;

/** A task title: trimmed, interior whitespace runs collapsed to single
 *  spaces, then non-empty + length-capped AFTER normalization. A title is a
 *  single-line string, and this is the ONE authority for that invariant —
 *  shared by create and both edit paths, so CLI `--title` and the UI textarea
 *  are normalized identically (no second normalization anywhere else). The
 *  length cap measures the canonical (collapsed) form, so raw runs of
 *  whitespace never count against it. */
export const TaskTitleSchema = z
  .string()
  .transform((s) => s.trim().replace(/\s+/g, ' '))
  .pipe(z.string().min(1).max(TASK_TITLE_MAX_LEN));
/** A task body/content: any string up to the cap, INCLUDING `""` (an empty
 *  body clears the description). Omission is normalized to `""` by the
 *  service — the stored/read `content` is always a string, never null. */
export const TaskBodySchema = z.string().max(TASK_CONTENT_MAX_LEN);

export const TaskAssigneeDtoSchema = z.object({
  type: TaskAssigneeTypeSchema,
  id: z.string(),
  /** Agent display_name or user name — denormalized for list render. */
  name: z.string(),
});
export type TaskAssigneeDto = z.infer<typeof TaskAssigneeDtoSchema>;

/** The task LINKED to a message via that message's immutable source provenance
 *  (`tasks.source_message_id == message.id`). Rendered as the message-row task
 *  chip. Semantics: "this message TRIGGERED/links task #N", NOT "this message IS
 *  a task".
 *
 *  VIEWER-INVARIANT by construction: it rides `MessageDto`, which fans out on the
 *  broadcast `conversation:{id}` scope (one payload → every subscriber), so it
 *  carries ONLY viewer-independent task facts. No per-viewer capability
 *  (`can_claim` etc.) lives here — those belong to per-viewer read surfaces.
 *  `linked_task` is a conversation-visible READ projection: it follows channel
 *  read access (public channel → every reader, member or not, sees the same
 *  chip; private → only members, and a non-member never reaches the read at
 *  all). It is NOT member-gated — the member boundary is the write/board axis,
 *  not this read. Mutations key off `task_id` (the task identity), NEVER the
 *  source message id. */
export const LinkedTaskDtoSchema = z.object({
  task_id: z.string(),
  task_number: z.number(),
  title: z.string(),
  status: TaskStatusSchema,
  assignee: TaskAssigneeDtoSchema.nullable(),
});
export type LinkedTaskDto = z.infer<typeof LinkedTaskDtoSchema>;

//
// A task is a FIRST-CLASS aggregate (its own `tasks` table), NOT a message with
// task metadata. `id` is the task identity everywhere DTOs/routes/events key on
// it; `task_number` is the per-workspace human/CLI resolver (#N monotonic).
// Ownership/operation scope is per-conversation (only agents in the task's
// conversation may claim/unclaim/update; other sessions get read-only aggregate
// visibility). Human UI is not assignee-restricted.

export const TaskConversationRefSchema = z.object({
  id: z.string(),
  title: z.string(),
});
export type TaskConversationRef = z.infer<typeof TaskConversationRefSchema>;

export const TaskCreatorDtoSchema = z.object({
  type: z.enum(['agent', 'user', 'system']),
  id: z.string().nullable(),
  name: z.string(),
});
export type TaskCreatorDto = z.infer<typeof TaskCreatorDtoSchema>;

/** The task's PLACE — the routing coordinate AND every placement-specific
 *  display coordinate, in ONE discriminated union. The canonical core
 *  (`conversation_id`) is FOLDED into each arm, so a `TaskPlace` is the task's
 *  complete location with no parallel top-level coordinate (contract §3).
 *
 *  - workspace arm: `conversation_id` (the routing coord) plus the workspace
 *    board namespace (`workspace_id`) and the channel's display title. It carried
 *    a `workspace_kind` word until #task:2075's stop-publish step; no board reader
 *    ever branched on it, and a container word on a Place DTO is over-projection
 *    for the same reason `workspace_name` is (contract §3). `workspace_name` is
 *    likewise deliberately ABSENT: it has zero production readers (the card
 *    renders `channel_name`; the container header resolves the workspace name from
 *    the live directory).
 *  - direct_pair arm: `conversation_id` plus ONLY the immutable `pair_key` (the
 *    v74 generated column's value read verbatim — its byte format is defined in
 *    the migration and never re-implemented in TS). NO title and NO display
 *    name: a DM stores no display title, and the per-viewer `dm:@other` label
 *    belongs to the DeliveryTarget formatter (#task:2021), never smuggled here.
 *
 *  Display/namespace ONLY — access control NEVER reads it (ACL always recomputes
 *  from `conversation_id` back to the canonical placement-participation
 *  authority; contract §4a: a Place DTO is not an authorization source). */
export const TaskPlaceSchema = z.discriminatedUnion('kind', [
  z.object({
    kind: z.literal('workspace'),
    conversation_id: z.string(),
    workspace_id: z.string(),
    /** The conversation's display title (a channel has one; a DM does not). */
    channel_name: z.string(),
  }),
  z.object({
    kind: z.literal('direct_pair'),
    conversation_id: z.string(),
    pair_key: z.string(),
  }),
]);
export type TaskPlace = z.infer<typeof TaskPlaceSchema>;

export const TaskSummaryDtoSchema = z.object({
  /** The task identity (`tasks.id`) — canonical key for REST paths, events,
   *  and mutations. */
  id: z.string(),
  task_number: z.number(),
  /** The task's place: the routing coordinate (`place.conversation_id`) folded
   *  together with the placement-specific display coordinates (see
   *  TaskPlaceSchema). Narrow on `place.kind` — workspace fields are
   *  structurally ABSENT on the pair arm. This is THE conversation coordinate;
   *  there is no separate top-level `channel_id` (contract §3). */
  place: TaskPlaceSchema,
  /** Reply-cluster root message id when the task's conversational footprint
   *  (its lifecycle system events) sits inside a thread cluster. NULL =
   *  main-line task. */
  thread_root_id: z.string().nullable(),
  status: TaskStatusSchema,
  title: z.string(),
  assignee: TaskAssigneeDtoSchema.nullable(),
  creator: TaskCreatorDtoSchema,
  created_at: z.number(),
  closed_at: z.number().nullable(),
});
export type TaskSummaryDto = z.infer<typeof TaskSummaryDtoSchema>;

/** Who last edited a task's DEFINITION (title/content) via `task edit`.
 *  `name` is denormalized server-side for render. Distinct from the task
 *  assignee/creator — this is the "edited by X" byline subject. */
export const TaskEditorDtoSchema = z.object({
  kind: z.enum(['agent', 'user']),
  id: z.string(),
  name: z.string(),
});
export type TaskEditorDto = z.infer<typeof TaskEditorDtoSchema>;

/** Viewer-resolved location of a message, for task navigation.
 *  `conversation_id` is the message's OWN conversation — a workspace channel or
 *  a DirectPair, whichever holds it (threads are reply clusters, not standalone
 *  conversations, so a threaded message resolves to its cluster's conversation).
 *  It is deliberately NOT named `channel_id`: on a pair-placed task the value is
 *  a pair conversation id, and "channel" would be a lie the caller cannot see
 *  through. */
export const TaskNavRefSchema = z.object({
  conversation_id: z.string(),
  thread_root_id: z.string().nullable(),
  message_id: z.string(),
});
export type TaskNavRef = z.infer<typeof TaskNavRefSchema>;

/** Server-side typed navigation discriminant for "View in conversation",
 *  resolved PER-VIEWER at detail read. The renderer branches ONLY on `kind` and
 *  never guesses a fallback:
 *   - `source`        → the task declares a source message the viewer can read.
 *   - `created_event` → no source; navigate to the persisted task-created system
 *                        event (the task's conversational footprint).
 *   - `unavailable`   → nothing to navigate to: no source AND no resolvable
 *                        event, OR a source exists but the viewer cannot read its
 *                        conversation. A DECLARED source NEVER silently falls
 *                        back to the event — losing access hides the button. */
export const TaskNavDtoSchema = z.discriminatedUnion('kind', [
  z.object({ kind: z.literal('source'), ref: TaskNavRefSchema }),
  z.object({ kind: z.literal('created_event'), ref: TaskNavRefSchema }),
  z.object({ kind: z.literal('unavailable') }),
]);
export type TaskNavDto = z.infer<typeof TaskNavDtoSchema>;

/** Full single-task read for the detail modal. Superset of TaskSummaryDto —
 *  the list stays title-only (no `content`), the detail carries the full body
 *  plus edit-provenance meta and the navigation ref. Fetched on demand when a
 *  card is opened; never batched into the board list. */
export const TaskDetailDtoSchema = TaskSummaryDtoSchema.extend({
  /** Full task body. */
  content: z.string(),
  /** Who last edited title/content, or null if never edited. */
  last_editor: TaskEditorDtoSchema.nullable(),
  /** When title/content was last edited (ms epoch), or null if never. This is
   *  the task's "updated" time — status/claim changes do NOT move it. */
  last_edited_at: z.number().nullable(),
  /** Immutable RAW provenance id (permanent 1:1 with the triggering message).
   *  NOT a foreign key — retained verbatim even if the message is later deleted,
   *  so `nav` resolves to `unavailable` rather than silently reclassifying the
   *  task to "no source". Never used directly by the client for navigation —
   *  that goes through `nav`. */
  source_message_id: z.string().nullable(),
  /** Server-resolved, per-viewer navigation for "View in conversation". A typed
   *  BRANCH discriminant (see TaskNavDtoSchema) — the client renders zero
   *  fallback logic: jump if a `ref` is present, hide the affordance on
   *  `unavailable`. */
  nav: TaskNavDtoSchema,
});
export type TaskDetailDto = z.infer<typeof TaskDetailDtoSchema>;

// GET /internal/agent/:aid/tasks (agent list). Human-side since #task:1891:
// nothing — the unbounded human `GET /api/tasks` retired once the board faces
// and the by-number point lookup covered every human read, so this is the AGENT
// contract alone. Widening it for a human surface would re-create the unbounded
// face under a new name.
export const ListTasksQuerySchema = z.object({
  workspace_id: z.string().optional(),
  workspace_name: z.string().optional(),
  /** A delivery address naming one conversation; excludes the other scope keys. */
  target: z.string().optional(),
  conversation_id: z.string().optional(),
  /** Filter by status. Empty / omitted = no filter. Multiple values are
   *  OR-ed (e.g. `?status=in_progress&status=in_review`). */
  status: z.array(TaskStatusSchema).optional(),
  /** Filter by task number. Multiple values are OR-ed
   *  (e.g. `?task_number=190&task_number=191`). */
  task_number: z.array(z.coerce.number().int().positive()).optional(),
  /** Filter by assignee agent id. Single-valued. */
  assignee_id: z.string().optional(),
  /** Cap the number of rows returned (server-side LIMIT). */
  limit: z.coerce.number().int().positive().max(500).optional(),
});
export type ListTasksQuery = z.infer<typeof ListTasksQuerySchema>;

// ── Human task board read faces (#task:1884) ─────────────────────────────
//
// The board is a HUMAN, CROSS-WORKSPACE surface (shiki, 2026-08-07): its
// baseline view is "All workspaces", so `workspace_id` is one optional
// narrowing axis — never a required scope. `ListTasksQuerySchema` above is the
// agent CLI's list contract, so NONE of the board's pagination
// lives there: the board gets its own query schemas and its own routes, and the
// agent contract is untouched by construction (not by a compatibility matrix).
//
// Three faces, three facts, one carrier each (see apiPaths.taskBoard*): a
// column window carries no totals, counts carry no items, facets carry no
// tasks. A paginated board therefore cannot hold two disagreeing copies of one
// number, and the loaded pages are never mistaken for the population.

/** Assignee axis. Omitted = any. `unassigned` means BOTH assignee columns are
 *  NULL (the aggregate splits one concept across `assignee_agent_id` /
 *  `assignee_user_id`, so "no assignee" must name both). */
export const TaskBoardAssigneeFilterTypeSchema = z.enum(['agent', 'user', 'unassigned']);
export type TaskBoardAssigneeFilterType = z.infer<typeof TaskBoardAssigneeFilterTypeSchema>;

/** Creator axis. Omitted = any. `system` is deliberately absent: a system
 *  creator has no id and is not a selectable person (the existing filter bar
 *  already skips it), so it is a value of the DATA, not of this filter. */
export const TaskBoardCreatorFilterTypeSchema = z.enum(['agent', 'user']);
export type TaskBoardCreatorFilterType = z.infer<typeof TaskBoardCreatorFilterTypeSchema>;

/** The board's TOP-LEVEL container axis (#task:2041). A task's owning
 *  conversation sits in exactly ONE human container, and the four-state tab
 *  `All | Workspace | Agent Space | Direct Messages` selects one (or, when
 *  omitted, their union):
 *    - `workspace`       — a conversation on a REGULAR workspace;
 *    - `agent_space`     — a conversation on an AGENT_HOME workspace;
 *    - `direct_messages` — a DirectPair (all of the viewer's pairs at once; a
 *                          single pair leaf is `channel_id`).
 *  Workspace and Agent Space are NOT distinguishable by placement alone — both
 *  are `placement_kind='workspace'` — so the split is an ownership
 *  (`owner_agent_id`) distinction, resolved authoritatively backend-side.
 *  Omitted = no container narrowing. This axis
 *  and `workspace_id` / `channel_id` stack as an intersection: choosing
 *  `agent_space` then a specific `workspace_id` narrows to that one Agent Space
 *  workspace. */
export const TaskBoardContainerSchema = z.enum(['workspace', 'agent_space', 'direct_messages']);
export type TaskBoardContainer = z.infer<typeof TaskBoardContainerSchema>;

/** The filter axes every board face shares. Kept as a raw field map (not a
 *  refined schema) so the three query schemas compose from ONE definition —
 *  a refined schema cannot be `.extend()`ed. */
const taskBoardFilterFields = {
  /** Optional workspace narrowing. Absent = the default All-workspaces board. */
  workspace_id: z.string().min(1).optional(),
  /** Query-filter narrowing by a task's owning conversation id. Despite the
   *  name, its value is the owning conversation id — the DTO no longer carries
   *  a `channel_id` field (the coordinate is `place.conversation_id`). Matches
   *  that conversation AND its threads: a thread task's conversation IS its
   *  channel; the reply cluster is `thread_root_id`. */
  channel_id: z.string().min(1).optional(),
  /** The board's TOP-LEVEL container axis (#task:2041):
   *  `workspace | agent_space | direct_messages`. See
   *  {@link TaskBoardContainerSchema}. Omitted = no container narrowing (the
   *  `All` tab spans every container the viewer can see). Stacks with
   *  `workspace_id` / `channel_id` as an intersection. */
  container: TaskBoardContainerSchema.optional(),
  assignee_type: TaskBoardAssigneeFilterTypeSchema.optional(),
  assignee_id: z.string().min(1).optional(),
  creator_type: TaskBoardCreatorFilterTypeSchema.optional(),
  creator_id: z.string().min(1).optional(),
} as const;

/** `{type,id}` is one value split across two query params, so the pairing is
 *  validated here — id alone is unsafe (agent and user ids share no namespace
 *  guarantee) and `unassigned` takes no id. */
function refineTaskBoardPersonAxes(
  v: {
    assignee_type?: TaskBoardAssigneeFilterType;
    assignee_id?: string;
    creator_type?: TaskBoardCreatorFilterType;
    creator_id?: string;
  },
  ctx: z.RefinementCtx,
): void {
  const needsId = v.assignee_type === 'agent' || v.assignee_type === 'user';
  if (needsId && v.assignee_id === undefined) {
    ctx.addIssue({
      code: z.ZodIssueCode.custom,
      path: ['assignee_id'],
      message: 'assignee_id is required when assignee_type is agent|user',
    });
  }
  if (!needsId && v.assignee_id !== undefined) {
    ctx.addIssue({
      code: z.ZodIssueCode.custom,
      path: ['assignee_id'],
      message: 'assignee_id requires assignee_type agent|user',
    });
  }
  if (v.creator_type !== undefined && v.creator_id === undefined) {
    ctx.addIssue({
      code: z.ZodIssueCode.custom,
      path: ['creator_id'],
      message: 'creator_id is required when creator_type is set',
    });
  }
  if (v.creator_type === undefined && v.creator_id !== undefined) {
    ctx.addIssue({
      code: z.ZodIssueCode.custom,
      path: ['creator_type'],
      message: 'creator_type is required when creator_id is set',
    });
  }
}

/** Default / maximum window size for one board column. The 20-row first screen
 *  is the product decision (#task:1883); the max bounds a reveal-driven
 *  catch-up read without letting a caller re-create the fetch-all. */
export const TASK_BOARD_DEFAULT_LIMIT = 20;
export const TASK_BOARD_MAX_LIMIT = 200;

/** GET /api/tasks/board/column — ONE status column's window. */
export const TaskBoardColumnQuerySchema = z
  .object({
    ...taskBoardFilterFields,
    /** Exactly one status: each column pages independently, so one column's
     *  window can never be eaten by another's rows. */
    status: TaskStatusSchema,
    /** Opaque keyset cursor from a previous response's `next_cursor`.
     *  Omitted = first window. */
    cursor: z.string().min(1).optional(),
    limit: CanonicalPositiveIntegerQuerySchema.pipe(
      z.number().max(TASK_BOARD_MAX_LIMIT),
    ).optional(),
  })
  .superRefine(refineTaskBoardPersonAxes);
export type TaskBoardColumnQuery = z.infer<typeof TaskBoardColumnQuerySchema>;

/** GET /api/tasks/board/counts — the authoritative per-status totals. */
export const TaskBoardCountsQuerySchema = z
  .object({ ...taskBoardFilterFields })
  .superRefine(refineTaskBoardPersonAxes);
export type TaskBoardCountsQuery = z.infer<typeof TaskBoardCountsQuerySchema>;

/**
 * GET /api/tasks/by-number — human point lookup by the board-scoped business
 * key. Both axes are REQUIRED: a task number is only unique per BOARD (the
 * allocation namespace is `board_key`, not the workspace — a workspace-less
 * DirectPair has its own `pair:…` board), so a lookup missing either half is
 * not a point lookup and must be rejected rather than widened into a scan.
 *
 * The board is named indirectly, by ANY `conversation_id` on it: the server
 * derives the board from the conversation's placement, then reads
 * `(board_key, task_number)`. That is why a DirectPair task — whose
 * `workspace_id` is NULL and so could never be named by a workspace key —
 * resolves here: its conversation maps to the `pair:…` board.
 *
 * The response is a single task or 404. `conversation_id` is the LOOKUP key
 * only, never an authorization claim — the server gates on the viewer's
 * visibility of the task's OWN conversation, and a task the viewer cannot see
 * is reported the same way as one that does not exist.
 */
export const TaskByNumberQuerySchema = z.object({
  conversation_id: z.string().min(1),
  task_number: CanonicalPositiveIntegerQuerySchema,
});
export type TaskByNumberQuery = z.infer<typeof TaskByNumberQuerySchema>;

/**
 * One column window. Every filter AND the viewer-visibility gate are applied
 * inside the query BEFORE its `LIMIT`, and `next_cursor` is derived by taking
 * `limit + 1` rows of that complete filtered population — nothing is dropped
 * after the page is cut. So a page can only be short because the column ended,
 * never because rows were filtered out of a full window.
 *
 * ORDER IS PART OF THIS CONTRACT: `items` are newest-first by
 * `(task_number DESC, id DESC)`, and successive pages continue STRICTLY after
 * the previous page's last item. Concatenating pages in request order therefore
 * yields the column in exactly that order, so a client renders in delivered
 * order and MUST NOT re-sort locally: `task_number` is only monotonic per
 * workspace (the default board is cross-workspace), so a client sorting by
 * number alone cannot reproduce this total order, and any local sort over the
 * loaded prefix would disagree with what the next page continues from.
 *
 * What the server does own is the CURSOR ENCODING (opaque) and PAGE LENGTH:
 * `next_cursor === null` stays the client's ONLY end-of-column signal, because
 * a FULL page can also be the last one.
 *
 * Carries NO total: totals are the counts face's fact.
 */
export const TaskBoardColumnDtoSchema = z.object({
  status: TaskStatusSchema,
  items: z.array(TaskSummaryDtoSchema),
  next_cursor: z.string().nullable(),
});
export type TaskBoardColumnDto = z.infer<typeof TaskBoardColumnDtoSchema>;

// Total by construction: derived from TaskStatusSchema, so a new status is
// covered without hand-listing it here (no "missing key = 0" compat branch).
const taskBoardCountShape = Object.fromEntries(
  TaskStatusSchema.options.map((s) => [s, z.number().int().nonnegative()]),
) as Record<TaskStatus, z.ZodNumber>;

/**
 * Every status's total under the requested filters — the ONE authority for a
 * board count. Always all five statuses, so a client never distinguishes
 * "zero" from "absent", and never maintains a count by `±1` on an event.
 * Also the count source for the titlebar Tasks badge (workspace-narrowed,
 * items not fetched), so that number stops being a second authority derived
 * from a full task download.
 */
export const TaskBoardCountsDtoSchema = z.object({
  counts: z.object(taskBoardCountShape),
});
export type TaskBoardCountsDto = z.infer<typeof TaskBoardCountsDtoSchema>;

/** A selectable person in a filter menu. */
export const TaskBoardPersonOptionSchema = z.object({
  type: TaskAssigneeTypeSchema,
  id: z.string(),
  name: z.string(),
});
export type TaskBoardPersonOption = z.infer<typeof TaskBoardPersonOptionSchema>;

/**
 * The filter menus' option sets, over the viewer's ENTIRE visible task
 * population.
 *
 * Deliberately takes NO filter params. Option sets must not shrink when a
 * filter is applied — otherwise selecting one person collapses the menu to
 * that person and the user cannot switch. This also matches the shipped
 * behavior exactly (both surfaces aggregate the options from the unfiltered
 * cross-workspace directory, not from the filtered list), so paging cannot
 * silently narrow a menu to "whoever appeared in the pages we happened to
 * load".
 */
export const TaskBoardFacetsDtoSchema = z.object({
  assignees: z.array(TaskBoardPersonOptionSchema),
  creators: z.array(TaskBoardPersonOptionSchema),
  /** Whether any visible task is unassigned — gates the "No assignee" option
   *  (shiki: otherwise it reads as a stray pseudo-person). */
  has_unassigned: z.boolean(),
});
export type TaskBoardFacetsDto = z.infer<typeof TaskBoardFacetsDtoSchema>;

// POST /internal/agent/:aid/tasks — create_tasks MCP tool
export const CreateTasksInputSchema = z.object({
  tasks: z
    .array(
      z.object({
        title: TaskTitleSchema,
        body: TaskBodySchema.optional(),
        /** Immutable provenance link: the message that TRIGGERED this task.
         *  A permanent identity key — a given message backs at most ONE task
         *  for its entire lifetime (any status, including done/closed). It is
         *  NOT a content source; title/content are the task's own definition.
         *  A second create from the same source fails with `source_conflict`
         *  and points at the existing task. */
        source_message_id: z.string().min(1).optional(),
      }),
    )
    .min(1),
  /** Agent-only: atomically self-claim the created task(s) (status
   *  in_progress + assignee = the creating agent) in the same write as the
   *  create. Ignored for human creates. In addition to `create_tasks`, an
   *  agent claim requires the `write_tasks` capability. */
  claim: z.boolean().optional(),
  /** The ONE stream the task is filed against, as an absolute address
   *  (#task:2021 P4-4) — required, because a board coordinate no one spelled is
   *  the ambient guess P4 exists to remove.
   *
   *  Filing a task is a single-stream operation, so the address carries BOTH
   *  degrees of freedom that used to be three fields (`conversation_id`,
   *  `target`, `reply_to_message_id`): the conversation decides the board, the
   *  row, its number, the `task_create` event and every downstream
   *  authorization (membership / lifecycle / capability / source), and the
   *  optional `/thread:<root_message_id>` suffix places the task's lifecycle
   *  notices under that reply cluster instead of the main line. Naming a
   *  conversation other than the caller's own IS the cross-conversation create;
   *  it is no longer a second flag. Unknown and unauthorized collapse to one
   *  answer, and a mid-cluster reply id is refused rather than widened to its
   *  root — both are properties of the one resolver, not of this door. */
  target: z.string().min(1),
});
export type CreateTasksInput = z.infer<typeof CreateTasksInputSchema>;

export const CreateTasksOpErrorSchema = z.enum([
  'conversation_not_found',
  'conversation_closed',
  'conversation_archived',
  'not_conversation_member',
  'agent_not_found',
  // `reply_target_not_found` is gone with the two-field target: a thread suffix
  // is validated as a ROOT OF THE ADDRESSED CONVERSATION by the one resolver,
  // which answers `thread_not_in_target` before the service runs. Keeping a
  // second code for the same fact would let the two drift, and the service no
  // longer looks the message up at all (#task:2021 P4-4).
  /** `source_message_id` referenced a message that does not exist. */
  'source_message_not_found',
  /** The actor cannot read the source message's conversation. */
  'source_not_accessible',
  /** The source message lives in a different conversation than the task
   *  (v1 contract: source and task must share one conversation). */
  'source_conversation_mismatch',
  /** A task already exists for this source message (permanent 1:1). The
   *  failure carries `existing_task` so the caller can point at it. */
  'source_conflict',
  /** `--claim` was combined with a cross-conversation create. Defined only for a
   *  conversation-anchored runtime whose `target` names a stream other than its
   *  anchor: that create is a dispatch — the task is filed unclaimed so a member of
   *  the destination conversation picks it up — so self-claiming to the caller is
   *  refused. An agent-scoped runtime has no anchor for a destination to differ
   *  from, so it is never refused here (#task:2150); its `--claim` is gated by the
   *  normal create_tasks/write_tasks capability (or, on a direct pair, participation)
   *  — the same authority the two-step create-then-by-id-claim spends. Decided at
   *  the route on the resolved destination; an address that resolves to nothing is
   *  answered by the resolver instead (#task:2021 P4-4). Either way no task or event
   *  is written, and the two refusals are not distinguishable by anyone who could
   *  not already see the destination. The human-readable reason lives only in the
   *  CLI mapper. */
  'cross_conversation_claim',
  /** The destination conversation is READ-ONLY: a direct-pair endpoint is
   *  gone (P0-e survivor-readonly). Nothing may write a task row or its
   *  system event into the pair — proven inside the write transaction. */
  'conversation_readonly',
]);
export type CreateTasksOpError = z.infer<typeof CreateTasksOpErrorSchema>;

/** The identity a create transaction authoritatively produces per task — the
 *  only fields guaranteed by the write itself, with no read-model projection.
 *  Both the agent batch result and the human panel create return exactly this,
 *  so a create ACK never depends on a post-commit re-read (which could report a
 *  committed write as a failure and drive a duplicate retry). */
export const CreatedTaskIdentitySchema = z.object({
  id: z.string(),
  task_number: z.number(),
  title: z.string(),
});
export type CreatedTaskIdentity = z.infer<typeof CreatedTaskIdentitySchema>;

export const CreateTasksSuccessSchema = z.object({
  tasks: z.array(CreatedTaskIdentitySchema),
});
export const CreateTasksFailureSchema = z.object({
  error: CreateTasksOpErrorSchema,
  /** Present when `error === 'source_conflict'`: the task that already owns
   *  the supplied source message (whatever its status). */
  existing_task: z
    .object({
      task_number: z.number(),
      id: z.string(),
      status: TaskStatusSchema,
    })
    .optional(),
});
export const CreateTasksResultSchema = z.union([
  CreateTasksSuccessSchema,
  CreateTasksFailureSchema,
]);
export type CreateTasksResult = z.infer<typeof CreateTasksResultSchema>;

// ── Human UI — POST /api/tasks (panel direct-create) ─────────────────────
// The human task panel creates a task directly, naming the target channel.
// It shares the SAME create semantics as the agent CLI (one
// `taskService.createTasks` — task row + `task_create` system event +
// `created_event_message_id` backfill in one txn); only the actor-policy edges
// differ (no `create_tasks` capability — that is an agent concept; no `claim` —
// a user actor's claim is ignored, humans create unassigned; no
// `source_message_id` — a panel create is sourceless, so its `nav` resolves to
// the created-event anchor). `conversation_id` is the EXPLICIT target (supplied
// by the writable-channel selector) — never derived from an ambient "current
// channel"; the server authorizes membership + open state fail-closed.
export const HumanCreateTaskInputSchema = z.object({
  title: TaskTitleSchema,
  body: TaskBodySchema.optional(),
  conversation_id: z.string().min(1),
});
export type HumanCreateTaskInput = z.infer<typeof HumanCreateTaskInputSchema>;

/** The typed failures a sourceless human create can surface, as a discriminated
 *  `{ success:false, code }` envelope the renderer classifies on `code` alone
 *  (never a reason string). The route's `canPostToConversation` gate collapses
 *  non-membership AND an unknown target into the SAME `not_conversation_member`
 *  (so existence never leaks) BEFORE the service runs; a member creating in a
 *  CLOSED or ARCHIVED conversation still reaches the service and gets the
 *  distinct `conversation_closed` / `conversation_archived`. `conversation_not_found`
 *  is retained for service completeness but is unreachable via the gated route.
 *  The agent-only `source_*` / `agent_not_found` codes cannot occur here. */
export const HumanCreateTaskErrorCodeSchema = z.enum([
  'conversation_not_found',
  'conversation_closed',
  'conversation_archived',
  'not_conversation_member',
]);
export type HumanCreateTaskErrorCode = z.infer<typeof HumanCreateTaskErrorCodeSchema>;

/** SUCCESS returns ONLY the identity the create transaction authoritatively
 *  produced (`id` / `task_number` / `title`) — the SAME shape the agent batch
 *  create returns. The panel opens the task modal by `id` and reads the full
 *  detail through the normal `GET /api/tasks/:id` path; the create ACK is never
 *  coupled to a post-commit projection read, so a committed write can never be
 *  reported as a failure (which would drive a duplicate retry on this sourceless,
 *  non-idempotent create). */
export const HumanCreateTaskResultSchema = z.union([
  z.object({ success: z.literal(true), task: CreatedTaskIdentitySchema }),
  z.object({
    success: z.literal(false),
    code: HumanCreateTaskErrorCodeSchema,
    reason: z.string().optional(),
  }),
]);
export type HumanCreateTaskResult = z.infer<typeof HumanCreateTaskResultSchema>;

// POST /internal/agent/:aid/tasks/claim — claim_tasks MCP tool
export const ClaimTasksInputSchema = z
  .object({
    /** Local-number batch: resolved on ONE explicit board (the workspace
     *  selector below, or the caller's ambient workspace). */
    task_numbers: z.array(PositiveJsSafeIntegerSchema).min(1).optional(),
    /** Global-identity batch (`tasks.id`, agent-scoped-session-v1 §4.5).
     *  EXACTLY ONE of `task_numbers` / `task_ids` per call — a mixed batch
     *  would need a board coordinate for the numbers while the ids forbid
     *  one, so the two selector families never share a call. */
    task_ids: z.array(z.string().min(1)).min(1).optional(),
    /** Optional workspace selector for cross-workspace task ops. When
     *  omitted, the target task is resolved in the actor's current
     *  conversation's workspace (the legacy default). When provided,
     *  the actor must already be a member of the resolved workspace.
     *  `workspace_id` and `workspace_name` are mutually exclusive.
     *  Only legal with `task_numbers`: a global id already determines its
     *  board, so a second coordinate could only be redundant or
     *  contradictory. */
    workspace_id: z.string().optional(),
    workspace_name: z.string().optional(),
  })
  .refine((v) => !(v.workspace_id && v.workspace_name), {
    message: 'workspace_id and workspace_name are mutually exclusive',
  })
  .refine((v) => (v.task_numbers !== undefined) !== (v.task_ids !== undefined), {
    message: 'provide exactly one of task_numbers or task_ids',
  })
  .refine((v) => !(v.task_ids && (v.workspace_id || v.workspace_name)), {
    message:
      'workspace selectors only apply to task_numbers; a task id already determines its board',
  });
export type ClaimTasksInput = z.infer<typeof ClaimTasksInputSchema>;

/** Typed code for claim-tool failures. Claim ONLY claims existing tasks by
 *  task number (+ optional workspace selector) — a task is a first-class
 *  aggregate, selected by its per-workspace number, never by a message id. To
 *  create+own a task from a message, use `task create --source-message-id
 *  --claim`. Cross-workspace task ops fail with `workspace_membership_required` /
 *  `unknown_workspace` / `ambiguous_workspace`. `task_done`, `already_claimed`
 *  etc. are stable; the free-form `reason` string is preserved alongside `code`. */
export const ClaimErrorCodeSchema = z.enum([
  'task_not_found',
  'task_done',
  'task_closed',
  'already_claimed',
  'not_self_assignment',
  'assignee_not_conversation_member',
  'conversation_not_found',
  'not_conversation_member',
  'agent_not_found',
  'unknown_workspace',
  'ambiguous_workspace',
  'workspace_membership_required',
  'capability_denied',
  'conversation_readonly',
]);
export type ClaimErrorCode = z.infer<typeof ClaimErrorCodeSchema>;

export const ClaimResultEntrySchema = z.object({
  success: z.boolean(),
  task_number: z.number().optional(),
  /** The task identity (`tasks.id`) of the claimed task; present on success. */
  id: z.string().optional(),
  reason: z.string().optional(),
  /** Typed failure code; present whenever `success === false`. */
  code: ClaimErrorCodeSchema.optional(),
  /** When `code === 'capability_denied'`, the missing capability bucket
   *  (doc §3.4b typed denial). */
  capability: AgentCapabilitySchema.optional(),
  /** If the task belongs to a different channel/thread, its id+title surface
   *  here so the caller can explain to the agent why claim failed. */
  owner_conversation: TaskConversationRefSchema.optional(),
  /** Workspace ids matching the supplied `workspace_name`, returned when
   *  `code === 'ambiguous_workspace'` so the caller can disambiguate. */
  workspace_candidates: z.array(z.string()).optional(),
});
export type ClaimResultEntry = z.infer<typeof ClaimResultEntrySchema>;

export const ClaimTasksResultSchema = z.object({
  results: z.array(ClaimResultEntrySchema),
});
export type ClaimTasksResult = z.infer<typeof ClaimTasksResultSchema>;

// POST /internal/agent/:aid/tasks/unclaim
export const UnclaimTaskInputSchema = z
  .object({
    task_number: PositiveJsSafeIntegerSchema.optional(),
    /** Global-identity selector (`tasks.id`) — exactly one of `task_number` /
     *  `task_id`. No workspace selector may accompany it (the id already
     *  determines its board). */
    task_id: z.string().min(1).optional(),
    /** Optional workspace selector — see ClaimTasksInputSchema. */
    workspace_id: z.string().optional(),
    workspace_name: z.string().optional(),
  })
  .refine((v) => !(v.workspace_id && v.workspace_name), {
    message: 'workspace_id and workspace_name are mutually exclusive',
  })
  .refine((v) => (v.task_number !== undefined) !== (v.task_id !== undefined), {
    message: 'provide exactly one of task_number or task_id',
  })
  .refine((v) => !(v.task_id && (v.workspace_id || v.workspace_name)), {
    message:
      'workspace selectors only apply to task_number; a task id already determines its board',
  });
export type UnclaimTaskInput = z.infer<typeof UnclaimTaskInputSchema>;

// POST /internal/agent/:aid/tasks/update-status
export const UpdateTaskStatusInputSchema = z
  .object({
    task_number: PositiveJsSafeIntegerSchema.optional(),
    /** Global-identity selector — see UnclaimTaskInputSchema. */
    task_id: z.string().min(1).optional(),
    status: TaskStatusSchema,
    /** Optional workspace selector — see ClaimTasksInputSchema. */
    workspace_id: z.string().optional(),
    workspace_name: z.string().optional(),
  })
  .refine((v) => !(v.workspace_id && v.workspace_name), {
    message: 'workspace_id and workspace_name are mutually exclusive',
  })
  .refine((v) => (v.task_number !== undefined) !== (v.task_id !== undefined), {
    message: 'provide exactly one of task_number or task_id',
  })
  .refine((v) => !(v.task_id && (v.workspace_id || v.workspace_name)), {
    message:
      'workspace selectors only apply to task_number; a task id already determines its board',
  });
export type UpdateTaskStatusInput = z.infer<typeof UpdateTaskStatusInputSchema>;

/** Typed code for unclaim / status mutation failures. */
export const TaskMutationErrorCodeSchema = z.enum([
  'task_not_found',
  'task_done',
  'wrong_conversation',
  'not_assignee',
  'invalid_transition',
  'already_at_status',
  'task_closed',
  'conversation_not_found',
  'not_conversation_member',
  'agent_not_found',
  'unknown_workspace',
  'ambiguous_workspace',
  'workspace_membership_required',
  'capability_denied',
  /** The task's conversation is READ-ONLY: a direct-pair endpoint is gone
   *  (P0-e survivor-readonly); mutations would write a system event into the
   *  pair. Proven inside the write transaction. */
  'conversation_readonly',
]);
export type TaskMutationErrorCode = z.infer<typeof TaskMutationErrorCodeSchema>;

// Simple success/failure envelope reused by unclaim / update-status.
export const TaskMutationResultSchema = z.object({
  success: z.boolean(),
  reason: z.string().optional(),
  /** Typed failure code; present whenever `success === false`. */
  code: TaskMutationErrorCodeSchema.optional(),
  /** When `code === 'capability_denied'`, the missing capability bucket
   *  (doc §3.4b typed denial). */
  capability: AgentCapabilitySchema.optional(),
  /** Workspace ids matching the supplied `workspace_name`, returned when
   *  `code === 'ambiguous_workspace'` so the caller can disambiguate. */
  workspace_candidates: z.array(z.string()).optional(),
});
export type TaskMutationResult = z.infer<typeof TaskMutationResultSchema>;

// Human UI — POST /api/tasks/:taskId/status
export const HumanUpdateTaskStatusInputSchema = z.object({
  status: TaskStatusSchema,
});
export type HumanUpdateTaskStatusInput = z.infer<typeof HumanUpdateTaskStatusInputSchema>;

// Human UI — POST /api/tasks/:taskId/claim (assign to a target agent).
// `agent_id` is REQUIRED: assignment and un-assignment are two writes with two
// meanings, and clearing the assignee is `POST /unclaim` — the single entry for
// that. An omitted `agent_id` used to fall through to "clear the assignee",
// which additionally advanced a `todo` task to `in_progress` (an unassigned
// task marked in progress); one meaning does not get two write paths.
export const HumanClaimTaskInputSchema = z.object({
  agent_id: z.string().min(1),
});
export type HumanClaimTaskInput = z.infer<typeof HumanClaimTaskInputSchema>;

// ── Edit (title / content) ────────────────────────────────────────────
// `task edit` mutates the task DEFINITION (title and/or content) as a plain
// last-write-wins UPDATE: the change is applied atomically by task id and the
// most recent successful write wins. There is NO optimistic-concurrency guard
// (no `expected_revision`, no `conflict` result): task edits are low-contention
// and the added round-trip/complexity would not pay for itself. A successful
// edit stamps last-editor; it never touches status/assignee. Both entry points
// reuse the shared title/body schemas.

// POST /internal/agent/:aid/tasks/edit — agent CLI `task edit`
export const EditTaskInputSchema = z
  .object({
    task_number: PositiveJsSafeIntegerSchema.optional(),
    /** Global-identity selector — see UnclaimTaskInputSchema. */
    task_id: z.string().min(1).optional(),
    title: TaskTitleSchema.optional(),
    body: TaskBodySchema.optional(),
    /** Optional workspace selector — see ClaimTasksInputSchema. */
    workspace_id: z.string().optional(),
    workspace_name: z.string().optional(),
  })
  .refine((v) => v.title !== undefined || v.body !== undefined, {
    message: 'provide at least one of title or body',
  })
  .refine((v) => !(v.workspace_id && v.workspace_name), {
    message: 'workspace_id and workspace_name are mutually exclusive',
  })
  .refine((v) => (v.task_number !== undefined) !== (v.task_id !== undefined), {
    message: 'provide exactly one of task_number or task_id',
  })
  .refine((v) => !(v.task_id && (v.workspace_id || v.workspace_name)), {
    message:
      'workspace selectors only apply to task_number; a task id already determines its board',
  });
export type EditTaskInput = z.infer<typeof EditTaskInputSchema>;

// Human UI — POST /api/tasks/:taskId/edit
export const HumanEditTaskInputSchema = z
  .object({
    title: TaskTitleSchema.optional(),
    body: TaskBodySchema.optional(),
  })
  .refine((v) => v.title !== undefined || v.body !== undefined, {
    message: 'provide at least one of title or body',
  });
export type HumanEditTaskInput = z.infer<typeof HumanEditTaskInputSchema>;

export const TaskEditErrorCodeSchema = z.enum([
  'task_not_found',
  'conversation_not_found',
  'not_conversation_member',
  'agent_not_found',
  'unknown_workspace',
  'ambiguous_workspace',
  'workspace_membership_required',
  'capability_denied',
  'conversation_readonly',
]);
export type TaskEditErrorCode = z.infer<typeof TaskEditErrorCodeSchema>;

export const TaskEditResultSchema = z.union([
  /** SUCCESS — the full, viewer-resolved task detail AFTER the edit, so an open
   *  detail view updates in place (new attribution/navigation) without a
   *  refetch. `task` is REQUIRED here — a successful edit that cannot return its
   *  detail is an invariant failure, not a legal `{success:true}` without a
   *  body. */
  z.object({
    success: z.literal(true),
    task: TaskDetailDtoSchema,
  }),
  /** Every typed failure (not-a-task / not-found / authz / workspace).
   *  Carries no `task`. */
  z.object({
    success: z.literal(false),
    code: TaskEditErrorCodeSchema,
    reason: z.string().optional(),
    capability: AgentCapabilitySchema.optional(),
    workspace_candidates: z.array(z.string()).optional(),
  }),
]);
export type TaskEditResult = z.infer<typeof TaskEditResultSchema>;

// Realtime payload — published on every task mutation (create / claim /
// unclaim / status / edit). Consumers refresh the list + the two live board
// surfaces (the global Tasks section and a conversation's inline Tasks tab).
// `edit` carries the new title; full content is fetched on demand via the
// detail read (the event stays thin).
//
// Published on TWO scopes: `conversation:<id>` (the conversation-visible read
// stream — public-aware, so members and non-members alike observe task changes
// in a channel they can read) and per-viewer `viewer:<userId>` for each human
// member of the task's conversation (the cross-conversation board, member-gated
// so a private channel's task never fans out beyond its members).
export const TaskChangedEventSchema = z.object({
  kind: z.enum(['create', 'claim', 'unclaim', 'status', 'edit']),
  /** Null for DirectPair-board tasks (P0-f): the event's routing keys are the
   *  conversation scopes below; workspace is a workspace-board-only fact. */
  workspace_id: z.string().nullable(),
  /** Reply-cluster root message id when the task's lifecycle events are
   *  placed inside a thread cluster. NULL = main-line. */
  thread_root_id: z.string().nullable(),
  conversation_id: z.string(),
  task_number: z.number(),
  /** The task identity (`tasks.id`) — the key consumers use to converge their
   *  task cache and the source message's `linked_task` projection. */
  task_id: z.string(),
  /** The task's immutable source message id (null when sourceless). Lets a
   *  consumer locate and live-patch that source message row's `linked_task`
   *  chip by message id — without scanning the cache. Viewer-invariant (a fixed
   *  provenance id). `linked_task` is a conversation-visible read projection, so
   *  a PUBLIC channel exposes it to members and non-members alike; only the
   *  per-viewer board fanout and task mutation are member-gated. */
  source_message_id: z.string().nullable(),
  status: TaskStatusSchema,
  assignee: TaskAssigneeDtoSchema.nullable(),
  title: z.string(),
  closed_at: z.number().nullable(),
});
export type TaskChangedEvent = z.infer<typeof TaskChangedEventSchema>;
