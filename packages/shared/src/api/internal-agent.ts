// AUTO-SPLIT from the former monolithic api/dto.ts (#task:963).
// Bounded context: internal-agent. Re-exported via api/index.ts → @silverhand/shared.
import { z } from 'zod';
import { HandleSchema } from '../handle.js';
import {
  ConversationPlacementKindSchema,
  ConversationRosterPolicySchema,
  ConversationVisibilitySchema,
} from '../db/schema.js';
import { ChannelTitleSchema } from './conversation.js';
import { UnresolvedReferencesSchema } from './message-reference.js';
import { MessageAttachmentIdsInputSchema, hasMessagePayload } from './_zod-helpers.js';
import { AttachmentSnapshotSchema } from './attachment.js';
import type { MessageAuthorDto } from './message-author.js';
import { ReactionAggregateDtoSchema } from './reactions.js';

/**
 * A message's placement INSIDE a conversation — the type that used to name where
 * a write landed — is GONE (#task:2021 P4-4).
 *
 * It had no domain left. `{kind:'channel'} | {kind:'thread', message_id}` only
 * ever meant something relative to a conversation the caller did not spell, so
 * both of its arms are now one absolute address: the send door took a `target`
 * string first, the read door second, and `task create` — the last holder, whose
 * conversation came from a separate `conversation_id` and whose thread came from
 * this type — third. A single `#conv:<id>[/thread:<root>]` says both, is
 * validated by ONE resolver, and cannot disagree with itself the way two fields
 * could.
 */
export const AgentStaleResolutionReasonSchema = z.enum([
  'unaffected',
  'revised',
  'obsolete',
  'force_send',
]);
export type AgentStaleResolutionReason = z.infer<typeof AgentStaleResolutionReasonSchema>;

export const AgentSendStaleResolutionReasonSchema = z.enum(['unaffected', 'revised', 'force_send']);
export type AgentSendStaleResolutionReason = z.infer<typeof AgentSendStaleResolutionReasonSchema>;

export const AgentSendMessageInputSchema = z
  .object({
    content: z.string(),
    /**
     * WHERE this message goes — an absolute address, and the only destination
     * coordinate on this door (#task:2021 P4-4).
     *
     * REQUIRED, with no ambient default: the sending conversation is where the
     * agent's credential lives, not where its words go, and an omitted target used
     * to mean "here" — the one spelling that becomes wrong the moment an agent
     * serves more than one stream. The CLI refuses the send before it reaches this
     * schema (`target_required`) so the correction can name the agent's own stream;
     * a request that reaches the server without one is a client bug, not a default.
     *
     * A STRING, parsed by `parseDeliveryTargetInput` — the grammar has exactly one
     * authority (`delivery-target.ts`) and this schema deliberately does not
     * restate any part of it. So no length bound either: the request body is
     * already bounded, and an address the grammar refuses is `target_malformed`,
     * which is the same answer a bound would have produced with a worse reason.
     */
    target: z.string().min(1),
    /** Correlates a retry/bypass send with a prior `message_stale` response.
     *  This is observability-only: missing or malformed stale-resolution
     *  telemetry must not become a send gate. */
    stale_event_id: z.string().min(1).optional(),
    stale_resolution_reason: AgentSendStaleResolutionReasonSchema.optional(),
    /** Explicit agent escape hatch for stale-context sends. Used by
     *  `silverhand message send --anyway`; the backend logs the bypass but
     *  does not run the freshness guard for this one send. */
    freshness_bypass: z.boolean().optional(),
    /** Optional attachment ids previously returned by `upload_file`.
     *  Server validates each id belongs to the same channel as the
     *  resolved target conversation. */
    attachment_ids: MessageAttachmentIdsInputSchema,
  })
  .refine(hasMessagePayload, {
    message: 'message must include text or at least one attachment',
    path: ['content'],
  });
export type AgentSendMessageInput = z.infer<typeof AgentSendMessageInputSchema>;

export const AgentResolveStaleMessageInputSchema = z.object({
  conversation_id: z.string().min(1),
  stale_event_id: z.string().min(1),
  resolution_reason: z.literal('obsolete'),
});
export type AgentResolveStaleMessageInput = z.infer<typeof AgentResolveStaleMessageInputSchema>;

export const AgentSendMessageOutputSchema = z.object({
  message_id: z.string(),
  /** WHERE the target resolved TO. Required, and the reason the send response is
   *  the daemon's only source of this fact: a readable address names a stream
   *  without identifying it, so an agent that sends by name would otherwise never
   *  learn the coordinate its own freshness cursor is keyed by. The daemon caches
   *  `address → this` and can then assert a stream on the next send to the same
   *  token; before that first response it sends loose, on purpose. */
  conversation_id: z.string(),
  /** Seq assigned by the target conversation allocator. The CLI uses this
   *  only to advance its local observed boundary after its own successful
   *  same-conversation send; it is not a room-current read. */
  message_seq: z.number().int().positive(),
  /** Reply-cluster root the sent message belongs to (§6.4): NULL for a
   *  main-line message, else the cluster's root message id. The daemon
   *  uses it to learn the reply-target stream of the new message and of
   *  the message it replied to. */
  thread_root_id: z.string().nullable(),
  /** The references in the sent body that addressed NOBODY (#task:2021 P4-5-b)
   *  — see {@link UnresolvedReferencesSchema}, whose single declaration this
   *  surface shares with the human send ACK. */
  unresolved_references: UnresolvedReferencesSchema,
});
export type AgentSendMessageOutput = z.infer<typeof AgentSendMessageOutputSchema>;

export const AgentCreateChannelInputSchema = z.object({
  /** The channel's title, WHICH IS ITS ADDRESS (#task:2016) — so the agent door
   *  states no rule of its own: it shares {@link ChannelTitleSchema} with the
   *  human door. An agent-created channel that a human could not have created,
   *  or that occupies an address no reference can spell, is not a thing this API
   *  is allowed to admit. */
  title: ChannelTitleSchema,
  context_summary: z.string().min(1),
  initial_prompt: z.string().min(1),
  /**
   * WHICH conversation is being FORKED — required, no ambient default
   * (#task:2021 P4-4).
   *
   * A channel is born into a workspace and starts with a roster, and this door's
   * only source for both is the conversation named here: its placement gives the
   * workspace, its agent members give the initial roster. So the address is not a
   * destination — nothing is posted into it — but it IS the operand, the same way
   * {@link AgentAddConversationMemberInputSchema}'s is, which is why it shares
   * the name and the grammar: a `target=` token copied out of a delivery header
   * works here too.
   *
   * A `/thread:<root>` suffix is refused (`thread_suffix_not_supported`): a reply
   * cluster has neither a placement of its own nor a member set, so forking one
   * would silently mean forking its parent — the ambient guess one level down.
   * A placement with no workspace (a DM pair) is refused with
   * `workspace_scope_required`: there is no workspace for the new channel to be
   * born into, and picking one would be that same guess.
   */
  target: z.string().min(1),
});
/** The REQUEST BODY an agent's CLI constructs — `z.input`, for the same reason as
 *  `CreateConversationChannelInput`: the title travels as the string the agent
 *  wrote, and the branded canonical pair exists only after the server parses it. */
export type AgentCreateChannelInput = z.input<typeof AgentCreateChannelInputSchema>;

/**
 * Agent-initiated conversation member add (#task:2021 P4-4).
 *
 * A door of its own rather than the human `AddConversationMemberByHandleInput`,
 * because the two differ in exactly the thing P4 is about: a human's request
 * carries the conversation in its PATH, while an agent's carries it here, as an
 * address. Sharing {@link HandleSchema} is the part that must not fork — the
 * candidate namespace is one namespace.
 */
export const AgentAddConversationMemberInputSchema = z.object({
  handle: HandleSchema,
  /**
   * WHICH conversation gains the member — required, no ambient default.
   *
   * A membership is a fact about a CONVERSATION, so a `/thread:<root>` suffix is
   * refused (`thread_suffix_not_supported`) rather than quietly dropped: a reply
   * cluster has no member set, and the way someone joins one is being referenced
   * or replying. One grammar all the same, so a `target=` token copied out of a
   * delivery header works here too — as long as it names the main line.
   */
  target: z.string().min(1),
});
export type AgentAddConversationMemberInput = z.infer<typeof AgentAddConversationMemberInputSchema>;

/** Agent voluntary thread follow/unfollow (reply-cluster §5 B / §8). The
 *  thread is identified by a *message id* — the cluster root, or any reply
 *  in it (the server normalizes a reply to its canonical root). */ export const AgentFollowThreadInputSchema =
  z.object({
    root_message_id: z.string().min(1),
  });
export type AgentFollowThreadInput = z.infer<typeof AgentFollowThreadInputSchema>;

export const AgentFollowThreadOutputSchema = z.object({
  /** Canonical cluster root the follow/unfollow resolved to. */
  thread_root_id: z.string(),
  /** follow: true when a NEW follower row was created (false = already
   *  following, no re-backfill). unfollow: true when a row was removed
   *  (false = was not following). */
  changed: z.boolean(),
});
export type AgentFollowThreadOutput = z.infer<typeof AgentFollowThreadOutputSchema>;

/** Structured detail attached to the backend `has_open_tasks` error so
 *  the CLI can render a list of the offending tasks without re-parsing
 *  the prose message. */
export const ThreadCloseOpenTaskDetailSchema = z.object({
  task_number: z.number().int().positive(),
  status: z.enum(['todo', 'in_progress', 'in_review']),
  title: z.string(),
  assignee_name: z.string().nullable(),
  assignee_agent_id: z.string().nullable(),
});
export type ThreadCloseOpenTaskDetail = z.infer<typeof ThreadCloseOpenTaskDetailSchema>;

const ReadHistoryOptionalSeqSchema = z.preprocess(
  (value) => (value === '' || value === undefined ? undefined : value),
  // #task:2341: sequence numbers are JS-safe integers on BOTH wires — a coerced
  // value beyond MAX_SAFE_INTEGER is rejected (not silently rounded), matching the
  // Rust `CoercedNumber` gate so the accepted value is byte-identical.
  z.coerce
    .number()
    .int()
    .nonnegative()
    .refine(Number.isSafeInteger, { message: 'must be a safe integer' })
    .optional(),
);

const ReadHistoryOptionalLimitSchema = z.preprocess(
  (value) => (value === '' || value === undefined ? undefined : value),
  z.coerce
    .number()
    .int()
    .positive()
    .max(100)
    .refine(Number.isSafeInteger, { message: 'must be a safe integer' })
    .optional()
    .default(50),
);

/** read_history target — RETIRED by #task:2021 P4-4. The read door now takes the
 *  same absolute address string as the send door and resolves it through the one
 *  resolver, so the three relative arms (`'self'`, `{conversation_id}`,
 *  `{thread_root_id}`) have no reader left. Deliberately not kept as an accepted
 *  alias: the `{thread_root_id}` arm normalized a mid-cluster id to its root,
 *  which is precisely the guess the address grammar refuses, so keeping it would
 *  leave two answers to "which stream is this token". */

export const AgentReadHistoryInputSchema = z
  .object({
    /**
     * WHICH STREAM to read — the same absolute address the send door takes, and
     * the only coordinate on this door (#task:2021 P4-4).
     *
     * REQUIRED. A read used to default to `'self'`, and to accept two spellings
     * (`{conversation_id}`, `{thread_root_id}`) that named a stream only
     * relative to the reading session. All three are gone: an agent that serves
     * more than one stream has no "here", and a read whose subject is implied is
     * the same ambient guess the send door just lost. A STRING for the same
     * reason as `AgentSendMessageInputSchema.target` — one grammar authority
     * (`delivery-target.ts`), nothing about it restated here.
     *
     * A read resolves with the SAME resolver at `intent: 'read'`, which is what
     * makes the two doors agree about what an address denotes: reads never
     * allocate (a `dm:@handle` typo cannot conjure an empty pair), and a thread
     * suffix must name a cluster ROOT — a mid-cluster message id is refused
     * rather than normalized, so `history` and `send` cannot disagree about
     * which stream one token means.
     */
    target: z.string().min(1),
    before_seq: ReadHistoryOptionalSeqSchema,
    after_seq: ReadHistoryOptionalSeqSchema,
    around_message_id: z.string().min(1).optional(),
    include_content: z.boolean().optional().default(false),
    include_system: z.boolean().optional().default(true),
    limit: ReadHistoryOptionalLimitSchema,
  })
  .refine(
    (value) =>
      [value.before_seq, value.after_seq, value.around_message_id].filter((v) => v !== undefined)
        .length <= 1,
    {
      message:
        'pass at most one of before_seq, after_seq, or around_message_id; omit all to read the latest window',
      path: ['before_seq'],
    },
  );
export type AgentReadHistoryInput = z.infer<typeof AgentReadHistoryInputSchema>;

export const MessageRefSchema = z.object({
  id: z.string(),
  /** `null` when the message lives in a conversation that is not workspace-placed
   *  (a DM pair — agent-scoped-session-v1 §4.1). A ref names WHERE a message
   *  is, and for such a placement the honest answer is "no workspace", not a
   *  borrowed coordinate: the alternative (echoing the reading session's
   *  workspace) would make a CC copy look like it lived on someone's board. */
  workspace_id: z.string().nullable(),
  conversation_id: z.string(),
  conversation_placement_kind: ConversationPlacementKindSchema,
  conversation_roster_policy: ConversationRosterPolicySchema,
  channel_id: z.string(),
  seq: z.number(),
  created_at: z.number(),
});
export type MessageRef = z.infer<typeof MessageRefSchema>;

/** Agent-facing author carrier for `message history` / `message search` output
 *  (#task:1793). A PURE thin projection of {@link MessageAuthorDto}: the SAME
 *  resolver (`resolveMessageAuthor`) decides active/deleted/unresolved/system,
 *  and this carrier is that result with the renderer-only fields dropped —
 *  `avatar_url` (a per-row cache-busted API URL) and `runtime` (a byline glyph
 *  hint) are never consumed by an agent reader, and this output lands directly
 *  in agent context (up to 100 rows per read), so carrying them is a pure token
 *  tax, not a free superset. A `null` sender means `from_kind='system'` (system
 *  is not an actor — mirrors `MessageAuthorDto`), so the reader tells system
 *  messages apart by a null sender rather than a magic `kind`.
 *
 *  Same discriminator discipline as the source DTO — `status` locks the valid
 *  field combinations at the schema layer, never inferred from which fields are
 *  null. The source's two `active` arms (split only to pin `runtime`) collapse
 *  to one here once `runtime` is dropped:
 *   - `active` / `deleted` — a stable `id` + `name` (handle) + `display_name`
 *     (all non-null); `deleted`'s values are the send-time snapshot. Only these
 *     carry a stable `id` and aggregate by it.
 *   - `unresolved` — everything null and non-aggregating (`id=null`): the
 *     identity is unrecoverable, NOT asserted deleted. `kind` survives so the
 *     reader can still say "Unknown agent" vs "Unknown user". */
const ResolvedSenderRefFields = {
  kind: z.enum(['agent', 'user']),
  id: z.string(),
  name: z.string(),
  display_name: z.string(),
} as const;
export const MessageSenderRefSchema = z.discriminatedUnion('status', [
  z.object({ status: z.literal('active'), ...ResolvedSenderRefFields }),
  z.object({ status: z.literal('deleted'), ...ResolvedSenderRefFields }),
  z.object({
    status: z.literal('unresolved'),
    kind: z.enum(['agent', 'user']),
    id: z.null(),
    name: z.null(),
    display_name: z.null(),
  }),
]);
export type MessageSenderRef = z.infer<typeof MessageSenderRefSchema>;

/** Pure projection from the resolved {@link MessageAuthorDto} to the agent-facing
 *  {@link MessageSenderRef}. No re-adjudication: `status` / `kind` / `id` are
 *  taken verbatim from the single resolver's result — only the renderer-only
 *  fields are dropped. `null` in ⇒ `null` out (system). */
export function senderRefFromAuthor(author: MessageAuthorDto | null): MessageSenderRef | null {
  if (author === null) return null;
  if (author.status === 'unresolved') {
    return {
      status: 'unresolved',
      kind: author.kind,
      id: null,
      name: null,
      display_name: null,
    };
  }
  return {
    status: author.status, // 'active' | 'deleted'
    kind: author.kind,
    id: author.id,
    name: author.name,
    display_name: author.display_name,
  };
}

export const ReadHistoryTargetRefSchema = z.object({
  conversation_id: z.string(),
  conversation_placement_kind: ConversationPlacementKindSchema,
  conversation_roster_policy: ConversationRosterPolicySchema,
  channel_id: z.string(),
  /** Set only when the address named a reply cluster (a
   *  `/thread:<root_message_id>` suffix): the canonical cluster root
   *  message id. `null` for every other target. Lets the caller tell
   *  the anchor root apart from its replies (the root carries
   *  `id === thread_root_id`; replies do not) without exposing
   *  per-message `thread_root_id`. */
  thread_root_id: z.string().nullable(),
  /** `null` for a target that is not workspace-placed (a DM pair) — same
   *  rule as {@link MessageRefSchema}. */
  workspace_id: z.string().nullable(),
  title: z.string(),
});
export type ReadHistoryTargetRef = z.infer<typeof ReadHistoryTargetRefSchema>;

export const ReadHistoryMessageSchema = z.object({
  message_ref: MessageRefSchema,
  sender: MessageSenderRefSchema.nullable(),
  snippet: z.string(),
  metadata: z.record(z.unknown()).nullable(),
  attachments: z.array(AttachmentSnapshotSchema).nullable(),
  /** Per-emoji reaction aggregates from the CALLING agent's viewer
   *  perspective (`viewer_reacted` is the agent's own). `null` when the
   *  message has no reactions. Carried on read-history (incl. the
   *  reply-cluster pull form) so an agent that can `message react` can
   *  also see the result — docs/reactions-design.md §5.2. */
  reactions: z.array(ReactionAggregateDtoSchema).nullable(),
  content: z.string().optional(),
});
export type ReadHistoryMessage = z.infer<typeof ReadHistoryMessageSchema>;

export const ReadHistoryAnchorSchema = z.union([
  z.object({ type: z.literal('head') }),
  z.object({ type: z.literal('before_seq'), seq: z.number() }),
  z.object({ type: z.literal('after_seq'), seq: z.number() }),
  z.object({ type: z.literal('around_message_id'), message_id: z.string() }),
]);
export type ReadHistoryAnchor = z.infer<typeof ReadHistoryAnchorSchema>;

export const AgentReadHistoryDtoSchema = z.object({
  target: ReadHistoryTargetRefSchema,
  anchor: ReadHistoryAnchorSchema,
  limit: z.number(),
  include_content: z.boolean(),
  include_system: z.boolean(),
  messages: z.array(ReadHistoryMessageSchema),
  has_more_before: z.boolean(),
  has_more_after: z.boolean(),
});
export type AgentReadHistoryDto = z.infer<typeof AgentReadHistoryDtoSchema>;

/** One workspace the calling agent currently has member access to.
 *  `owner_agent_id` is WHOSE container this is — the home agent of an agent
 *  space, `null` for a regular workspace. It replaced a published container
 *  word (`regular | agent_space`) in #task:2075's stop-publish step, and the
 *  decision that word recorded still holds, now without a word at all: the
 *  ubiquitous language does NOT fork by audience. This agent face and the
 *  human faces (`ViewerWorkspaceDto`, `SearchWorkspaceRef`) carry the SAME
 *  field, so there is no dialect to translate on a cross-surface inference —
 *  #task:2069 converted the two vocabularies into one, and this ticket
 *  retired the vocabulary instead of translating it.
 *
 *  The field stays (rather than being deleted like the Place DTOs' word)
 *  because a real reader has a real question the rest of the DTO cannot
 *  answer: `silverhand workspace list` hands this JSON to an agent verbatim,
 *  and "which of these is my own home" is exactly what decides whether the
 *  entry's MEMORY.md is that agent's L1 or a workspace L2. Recognize it with
 *  `isAgentHomeWorkspaceOf`/`isAgentHomeWorkspaceId`, never by re-deriving a
 *  container word.
 *  `agent_dir` is the agent's working directory inside this
 *  workspace — for an agent space this is the agent's L1 home itself,
 *  for `regular` it is the deterministic agent-centric bundle
 *  `<agent home>/workspaces/<workspace_id>` (relocation-v1 §3.2/§7:
 *  computed from ids, never read from a DB column or registry
 *  binding). `null` means UNAVAILABLE ON THIS COMPUTER: the coordinate
 *  is not a usable working directory yet — the membership is a pure DB
 *  write until the agent first spawns there. Never a fabricated path;
 *  it resolves on that first spawn. */
export const AgentListedWorkspaceSchema = z.object({
  id: z.string(),
  name: z.string(),
  owner_agent_id: z.string().nullable(),
  archived: z.boolean(),
  updated_at: z.number(),
  agent_dir: z.string().nullable(),
});
export type AgentListedWorkspace = z.infer<typeof AgentListedWorkspaceSchema>;

export const AgentListWorkspacesDtoSchema = z.object({
  workspaces: z.array(AgentListedWorkspaceSchema),
});
export type AgentListWorkspacesDto = z.infer<typeof AgentListWorkspacesDtoSchema>;

/** Optional query for `list_channels`. When `workspace_id` is omitted,
 *  the route defaults to the source conversation's workspace. */
export const AgentListChannelsQuerySchema = z.object({
  workspace_id: z.string().min(1).optional(),
});
export type AgentListChannelsQuery = z.infer<typeof AgentListChannelsQuerySchema>;

/** Query for the peer-profile read (#task:1823). `handle` is the TARGET
 *  agent's handle on the SHARED handle grammar (`HandleSchema` — a leading
 *  `@` is tolerated so an agent can paste a mention verbatim, and the route
 *  normalizes it away before resolving); `workspace_id` names the workspace
 *  the co-membership is evaluated in and defaults to the source
 *  conversation's workspace, exactly like `list_channels`. */
export const AgentPeerProfileQuerySchema = z.object({
  handle: HandleSchema,
  workspace_id: z.string().min(1).optional(),
});
export type AgentPeerProfileQuery = z.infer<typeof AgentPeerProfileQuerySchema>;

export const AgentListedChannelSchema = z.object({
  id: z.string(),
  title: z.string(),
  roster_policy: ConversationRosterPolicySchema,
  /** Channel visibility. Authoritative on every row; a caller must not infer
   *  it from roster policy or list position. Fixed-roster rows are private. */
  visibility: ConversationVisibilitySchema,
  archived: z.boolean(),
  /** Whether the listed agent is a member of the conversation. Authoritative:
   *  computed from the membership relation, never inferred from `visibility`,
   *  roster policy, or list position. `visibility: 'public'` with `is_member: false`
   *  is the public-but-not-joined discovery state. Discovery grants no read,
   *  write, roster, or delivery; joining remains a separate action. */
  is_member: z.boolean(),
  updated_at: z.number(),
});
export type AgentListedChannel = z.infer<typeof AgentListedChannelSchema>;

export const AgentListChannelsDtoSchema = z.object({
  workspace_id: z.string(),
  channels: z.array(AgentListedChannelSchema),
});
export type AgentListChannelsDto = z.infer<typeof AgentListChannelsDtoSchema>;

/** Where a search actually landed. OUTPUT ONLY — an input never carries
 *  it. The caller passes at most one selector and the scope is derived
 *  from that, so there is no mode+selector pair that can disagree. */
export const AgentSearchMessagesScopeSchema = z.enum(['current', 'workspace', 'conversation']);
export type AgentSearchMessagesScope = z.infer<typeof AgentSearchMessagesScopeSchema>;

const AgentSearchMessagesLimitSchema = z.preprocess(
  (value) => (value === '' || value === undefined ? undefined : value),
  z.coerce
    .number()
    .int()
    .positive()
    .max(100)
    .refine(Number.isSafeInteger, { message: 'must be a safe integer' })
    .optional()
    .default(50),
);

export const AgentSearchMessagesInputSchema = z
  .object({
    q: z.string().trim().min(1).max(200),
    /** Selector-only: the search range is whichever selector is present,
     *  and no selector means the calling conversation. `current_workspace`
     *  is `true`-or-absent on purpose — its presence IS the signal, so an
     *  explicit `false` would be a second spelling of the default. */
    current_workspace: z.literal(true).optional(),
    /** Both id selectors are `.min(1)`: a present-but-empty selector is
     *  refused, not coerced to absent. Blank-as-absent is right for the
     *  human search FILTERS (`MessageSearchInput`, where an untouched form
     *  field means "no filter"), but here the key IS the range — treating
     *  `''` as absent would answer a caller who named a conversation with
     *  the calling conversation instead. */
    workspace_id: z.string().min(1).optional(),
    conversation_id: z.string().min(1).optional(),
    include_system: z.boolean().optional().default(true),
    limit: AgentSearchMessagesLimitSchema,
  })
  // Strict on purpose: the retired `scope` / `channel_id` keys must fail
  // loudly. Stripping them would silently narrow a caller's intended
  // workspace/channel search down to the calling conversation — the same
  // silent last-wins the selector-only model exists to remove.
  .strict()
  .superRefine((v, ctx) => {
    const selectors = [
      v.current_workspace ? 'current_workspace' : null,
      v.workspace_id ? 'workspace_id' : null,
      v.conversation_id ? 'conversation_id' : null,
    ].filter((name): name is string => name !== null);
    if (selectors.length > 1) {
      ctx.addIssue({
        code: z.ZodIssueCode.custom,
        message:
          `pass at most one of current_workspace, workspace_id, ` +
          `conversation_id (got ${selectors.join(', ')}); omit all to ` +
          `search the calling conversation`,
        path: [selectors[1]!],
      });
    }
  });
export type AgentSearchMessagesInput = z.infer<typeof AgentSearchMessagesInputSchema>;

export const AgentSearchMessageHitSchema = z.object({
  message_ref: MessageRefSchema,
  sender: MessageSenderRefSchema.nullable(),
  excerpt: z.string(),
  match_offset: z.number().int().nonnegative(),
});
export type AgentSearchMessageHit = z.infer<typeof AgentSearchMessageHitSchema>;

export const AgentSearchMessagesDtoSchema = z.object({
  query: z.string(),
  scope: AgentSearchMessagesScopeSchema,
  workspace_id: z.string().nullable(),
  conversation_id: z.string().nullable(),
  include_system: z.boolean(),
  limit: z.number(),
  hits: z.array(AgentSearchMessageHitSchema),
});
export type AgentSearchMessagesDto = z.infer<typeof AgentSearchMessagesDtoSchema>;

//
// The CLI fires a one-shot trace at the cli-trace endpoint when it
// fails BEFORE any business HTTP call (parse / strict-flag / unknown
// noun). Backend logs it as `cli.preflight_failure` so we can see the
// typo Top-N without the agent ever realizing the trace exists.
//
// Strict but lenient: every field is optional so a half-formed CLI
// invocation can still be reported. The backend never echoes anything
// back beyond `{ ok: true }`.

export const CliTraceInputSchema = z
  .object({
    /** `silverhand <noun>...` if the noun parsed. */
    noun: z.string().max(64).optional(),
    /** `silverhand <noun> <verb>...` if both parsed. */
    verb: z.string().max(64).optional(),
    /** Names of the flags observed on the command line. NEVER values. */
    flags_seen: z.array(z.string().max(64)).max(64).optional(),
    /** Stable error code from the CLI, e.g. `validation_error`. */
    error_code: z.string().max(64).optional(),
    /** Optional descriptive sub-classification of `error_code`, e.g.
     *  `flag_removed`, `unexpected_arguments`. Wide string here (not a
     *  closed enum) so the CLI can introduce new subkinds without a
     *  shared schema bump; the CLI-side `ErrorSubkind` union is the
     *  source of truth. NEVER values. */
    error_subkind: z.string().max(64).optional(),
    /** First line of the user-facing error message (truncated). */
    error_message: z.string().max(500).optional(),
    /** Wall time from CLI start to error throw, in milliseconds. */
    duration_ms: z.number().nonnegative().optional(),
  })
  .strict();
export type CliTraceInput = z.infer<typeof CliTraceInputSchema>;

export const CliTraceOutputSchema = z.object({ ok: z.literal(true) });
export type CliTraceOutput = z.infer<typeof CliTraceOutputSchema>;

export const ConversationLogDebugInputSchema = z
  .object({
    enabled: z.boolean(),
  })
  .strict();
export type ConversationLogDebugInput = z.infer<typeof ConversationLogDebugInputSchema>;

export const ConversationLogDebugStatusSchema = z.object({
  conversationId: z.string(),
  enabled: z.boolean(),
  expiresAt: z.number().int().nullable(),
  daemonSynced: z.boolean().optional(),
});
export type ConversationLogDebugStatus = z.infer<typeof ConversationLogDebugStatusSchema>;
