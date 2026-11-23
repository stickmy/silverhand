import { z } from 'zod';

export const RuntimeKindSchema = z.enum([
  'claude',
  'codex',
  'cursor',
  'gemini',
  'qoder',
  'opencode',
  'pi',
  'grok',
  'antigravity',
]);
export type RuntimeKind = z.infer<typeof RuntimeKindSchema>;

/** Immutable cognitive/runtime ownership selected when an Agent is created. */
export const AgentSessionScopeSchema = z.enum(['conversation', 'agent']);
export type AgentSessionScope = z.infer<typeof AgentSessionScopeSchema>;

// Reasoning effort is a bounded, opaque, lowercase TOKEN shared by persistence,
// the HTTP DTOs, and the daemon `runtime_start` wire. The schema bounds structure
// only: values belong to each runtime's CLI, which decides what a model accepts.
// The bound keeps a malformed value out of CLI config overrides and lets a daemon
// frame a value it has never seen, so a new effort never needs a protocol bump.
// `null` (unset) is distinct from `'none'`: unset passes no reasoning override.
export const ReasoningEffortTokenSchema = z
  .string()
  .min(1)
  .max(32)
  .regex(/^[a-z][a-z0-9_-]*$/);
export type ReasoningEffortToken = z.infer<typeof ReasoningEffortTokenSchema>;

// Domain-readable alias used at DTO / row / catalog field sites. IDENTICAL to
// `ReasoningEffortToken` — it carries NO global legality at the type level: any
// structurally-valid token is a valid `ReasoningEffort`, and whether a specific
// runtime/model accepts it is a runtime-catalog decision, not a type-system one.
export type ReasoningEffort = ReasoningEffortToken;

/** Codex service tier id (`default`, `priority`, `ultrafast`, ...). The CLI owns
 *  the set; null delegates to its default. */
export const ServiceTierSchema = ReasoningEffortTokenSchema;
export type ServiceTier = z.infer<typeof ServiceTierSchema>;

export const HostKindSchema = z.enum(['local', 'remote']);
export type HostKind = z.infer<typeof HostKindSchema>;

export const HostProvisioningSourceSchema = z.enum(['app_install', 'user_invite', 'system']);
export type HostProvisioningSource = z.infer<typeof HostProvisioningSourceSchema>;

export const HostCredentialKindSchema = z.enum(['long_lived', 'bootstrap']);
export type HostCredentialKind = z.infer<typeof HostCredentialKindSchema>;

/**
 * WHERE a conversation lives, and therefore how its participant set is derived
 * (agent-scoped-session-v1 §3/§4.1). Exhaustive and mutually exclusive:
 * `workspace` keeps explicit membership rows; `direct_pair` derives its
 * participants from the placement itself. Migration v69 introduces the axis with
 * only `workspace` representable — the placement-specific columns arrive with the
 * placements themselves (§8 P0-b / P3).
 *
 * There were three placements until #task:2072: `agent_inbox` was a
 * single-participant container minted one per agent, and it is retired (v84/v85)
 * because waking an agent must happen in a conversation that has a SECOND
 * participant. Every placement here now has at least two.
 *
 * THE KEYING RULE, stated here ONCE because three read points got it wrong
 * independently (#task:2074 / #task:2076 / #task:2077, all one mis-keying):
 * a conversation's IDENTITY — what it IS, what it is called, which glyph names it
 * — is a function of PLACEMENT. Its CAPABILITIES — may members be added, may the
 * roster change, does unread arrive directed-grade — are a function of
 * `roster_policy`. Every read point must say which of the two it is asking, and a
 * `roster_policy` read may never stand in for the identity question: a fixed
 * roster is a CONSEQUENCE of some placements, never the definition of one (an
 * agent home's DM channel and its Onboarding channel are workspace-placed with a
 * fixed roster, and #task:1373 says a workspace conversation is a channel
 * whatever its roster).
 *
 * The corollary that keeps costing a wire field: a read face RESTRICTED to
 * `workspace` placements has no identity branch at all — not a branch keyed on
 * something better. Publishing this word to such a face ships a constant. That is
 * why `ActivityItemDto` carries neither this word nor a roster bit
 * (`api/attention.ts`), while `AgentRuntimeParticipationSchema` — a face that
 * genuinely admits `direct_pair` — carries this word and needs it
 * (`api/agent.ts`).
 */
export const ConversationPlacementKindSchema = z.enum(['workspace', 'direct_pair']);
export type ConversationPlacementKind = z.infer<typeof ConversationPlacementKindSchema>;

/**
 * WHETHER a conversation's roster can change: `open` (members may be added or
 * removed) | `fixed` (the participant set is determined at creation and
 * immutable). Independent of placement.
 */
export const ConversationRosterPolicySchema = z.enum(['open', 'fixed']);
export type ConversationRosterPolicy = z.infer<typeof ConversationRosterPolicySchema>;

/**
 * WHICH KIND OF PRINCIPAL a `direct_pair` endpoint names (migration v73,
 * agent-scoped-session-v1 §4.2). It selects the table the sibling
 * `endpoint_*_id` must be looked up in — a conditional reference no foreign key
 * can express, which is why the DB validates it with a trigger subquery instead.
 *
 * Deliberately NOT the same enum as a message's `from_kind` or a thread
 * follower's `principal_kind` even though the two members coincide today: this
 * one is a COORDINATE SYSTEM for the endpoint columns, and widening it means
 * teaching the birth trigger a new table.
 */
export const ConversationEndpointKindSchema = z.enum(['human', 'agent']);
export type ConversationEndpointKind = z.infer<typeof ConversationEndpointKindSchema>;

/**
 * Channel visibility. `private` is the historical, membership-
 * gated semantic — only explicit `conversation_users` members can read or
 * interact. `public` widens READ (browse content) to any human workspace
 * member while leaving every interaction gated on membership (join to
 * participate). Meaningful only for an OPEN roster; a fixed-roster
 * conversation is always `private` (enforced by a DB CHECK — introduced in
 * migration v58 against `kind`, rewritten in v69 to read `roster_policy`).
 */
export const ConversationVisibilitySchema = z.enum(['private', 'public']);
export type ConversationVisibility = z.infer<typeof ConversationVisibilitySchema>;

export const ConversationStatusSchema = z.enum([
  'starting',
  'active',
  'idle',
  'archived',
  'crashed',
]);
export type ConversationStatus = z.infer<typeof ConversationStatusSchema>;

export const ConversationAgentStatusSchema = z.enum(['idle', 'starting', 'active', 'crashed']);
export type ConversationAgentStatus = z.infer<typeof ConversationAgentStatusSchema>;

export const FromKindSchema = z.enum(['user', 'agent', 'system']);
export type FromKind = z.infer<typeof FromKindSchema>;

export const TaskStatusSchema = z.enum(['todo', 'in_progress', 'in_review', 'done', 'closed']);
export type TaskStatus = z.infer<typeof TaskStatusSchema>;

export const TaskAssigneeTypeSchema = z.enum(['agent', 'user']);
export type TaskAssigneeType = z.infer<typeof TaskAssigneeTypeSchema>;

/**
 * The persisted activity-row alphabet. NOT the same alphabet as
 * `PARSED_EVENT_KINDS` (the driver's), and deliberately so: this one is keyed
 * on "what this runtime did or experienced during its run", whoever observed
 * it. Most rows are the driver's parsed output, but `parsed_event_dropped`
 * (daemon-observed wire loss), `message_stale` (daemon-relayed backend
 * delivery verdict, #task:2099) and `message_received` (daemon-authored inbound
 * delivery fact, #task:2146) are authored by the daemon and have no
 * `ParsedEvent` counterpart. Adding one here does NOT license adding it there
 * — a synthetic parsed event would claim the runtime said something it didn't.
 */
export const ConversationEventKindSchema = z.enum([
  'session_init',
  'thinking',
  'text',
  'tool_call',
  'tool_output',
  'file_change',
  'shell',
  'stderr',
  'compaction_started',
  'compaction_finished',
  'turn_end',
  'api_response',
  'error',
  'unknown',
  'parsed_event_dropped',
  'message_stale',
  'message_received',
]);
export type ConversationEventKind = z.infer<typeof ConversationEventKindSchema>;

/**
 * D4.d v33 (additive): agent-materialization lifecycle on the
 * server side. Existing rows backfill to 'active' (they went
 * through the legacy backend-direct-fs path so the L1 already
 * exists). New rows START in 'pending_materialization' and flip
 * to 'active' on daemon ack (L3a) or 'failed' on typed error /
 * timeout (L3b). See `docs/daemon-outbound-protocol.md`
 * (§ "agent_materialize command").
 */
export const AgentMaterializationStatusSchema = z.enum([
  'pending_materialization',
  'active',
  'failed',
]);
export type AgentMaterializationStatus = z.infer<typeof AgentMaterializationStatusSchema>;

export interface HostRow {
  id: string;
  name: string;
  kind: HostKind;
  /** User that owns this host. NULL only for pre-claim legacy/local rows. */
  user_id: string | null;
  /** Stable local install id used to deduplicate desktop local-host claims. */
  installation_id: string | null;
  last_seen_at: number | null;
  revoked_at: number | null;
  created_at: number;
  /** User-managed computer lifecycle marker. NULL means pending attach;
   *  non-null means this host has completed at least one authenticated daemon
   *  attachment. */
  activated_at: number | null;
  /** User confirmation marker. User-invited computers stay setup drafts until
   *  the modal finalizes them; runtime readiness composes this with
   *  activated_at, revoked_at, and owner ACL. */
  finalized_at: number | null;
  /** Lifecycle/origin metadata only. Must not gate runtime capability,
   *  placement, routing, or ACL decisions. */
  provisioning_source: HostProvisioningSource;
  /** v36 (#task:808): user-facing rename target for the ComputerPane
   *  / host registry view. `name` stays the stable normalized identifier
   *  used by `claimOrCreateLocal` and future ref resolvers. Renderer
   *  reads `display_label ?? name`. NULL on rows claimed before this
   *  column existed (and after a rename clears the label back to default).
   *  Length 1-64; CJK/spaces allowed (display field, not an identifier). */
  display_label: string | null;
}

export interface HostCredentialRow {
  id: string;
  host_id: string;
  token_hash: string;
  kind: HostCredentialKind;
  expires_at: number | null;
  consumed_at: number | null;
  revoked_at: number | null;
}

export interface AgentRow {
  id: string;
  name: string;
  display_name: string;
  runtime: RuntimeKind;
  /** P0-d dormant authority; product creation activates `agent` in P2. */
  session_scope: AgentSessionScope;
  model: string | null;
  reasoning_effort: ReasoningEffort | null;
  service_tier: ServiceTier | null;
  host_id: string;
  description: string | null;
  /** v26: human who created the agent (a `users.id`). Source of truth
   *  for `canEditAgent` — only the creator and the agent itself may
   *  edit profile / replace avatar. NOT NULL with `ON DELETE RESTRICT`
   *  (deleting the user is blocked while their agents still reference
   *  it; v2 multi-human flows must reassign or hard-delete first). */
  created_by_user_id: string;
  /** v26: storage key inside the configured BlobFileStorage; NULL when
   *  no avatar is set. Coupled with `avatar_content_type` via CHECK
   *  (mirrors `users` v21). */
  avatar_storage_key: string | null;
  avatar_content_type: string | null;
  /** v33 (D4.d): agent-home materialization lifecycle on the
   *  server side. Existing rows backfill to 'active'; new rows
   *  start at 'pending_materialization' and flip on daemon ack
   *  (L3a → 'active') or typed error / timeout (L3b → 'failed').
   *  Default-hidden from list/get; explicit `?include_failed=1`
   *  opt-in is ACL/creator-scoped (see §4.1 / §7.3). */
  materialization_status: AgentMaterializationStatus;
  /** v33 (D4.d): populated only when `materialization_status='failed'`.
   *  Mirrors the wire `AgentMaterializeErrorCodeSchema` plus
   *  server-side codes (`materialize_timeout` /
   *  `host_attach_pending` / `dependent_row_insert_failed` /
   *  `backend_crash_during_materialize`). Kept as `string | null`
   *  in shared schema — the route layer pins the exact code set
   *  it produces; this row column intentionally accepts the
   *  superset so the boot reconciler / future codes do not need
   *  a coordinated migration. */
  materialization_failed_code: string | null;
  /** v33 (D4.d): epoch ms set together with
   *  `materialization_failed_code`; NULL while the row is in
   *  `pending_materialization` or `active`. */
  materialization_failed_at: number | null;
  /** v33 (D4.d): client-supplied opaque retry key, 1..64 chars,
   *  `[A-Za-z0-9_-]`, stored verbatim. Unique per creator (NOT
   *  globally) — `idx_agents_idempotency_key_per_user` on
   *  `(created_by_user_id, idempotency_key) WHERE NOT NULL`. All
   *  lookup paths MUST filter by `(created_by_user_id, idempotency_key)`
   *  — never by key alone (Cindy round-2 P1 #5). */
  idempotency_key: string | null;
  /** Target runtime while a durable runtime transition owns the agent. */
  runtime_transition_target: RuntimeKind | null;
  /** Epoch ms paired with `runtime_transition_target`; internal, not a DTO field. */
  runtime_transition_started_at: number | null;
  /** Epoch ms durable terminal-deletion intent and attempt CAS token. */
  deletion_started_at: number | null;
  /** v82 (#task:2049): durable per-agent presence clock. NULL = never active.
   *  Written ONLY by the backend at admitted runtime-activity edges (parsed
   *  event / delivery consumed) with the BACKEND clock, moved forward by
   *  monotonic max. The host-authored frame `ts` is NEVER written here (host
   *  clock is untrusted for a durable, monotonic value). Feeds the `/presence`
   *  read face's `away_at`. */
  last_activity_at: number | null;
  created_at: number;
  updated_at: number;
}

export interface UserRow {
  id: string;
  /** Mention name. Decided once at registration (V1 first launch / V2
   *  signup) and never editable after — no rename endpoint, no service
   *  method. Globally unique across `users.name` ⊕ `agents.name` — see
   *  migration v21 cross-namespace uniqueness triggers. */
  name: string;
  display_name: string;
  /** Storage key inside the configured BlobFileStorage; NULL when no
   *  avatar is set. Coupled with `avatar_content_type` via CHECK. */
  avatar_storage_key: string | null;
  avatar_content_type: string | null;
  /** Canonical login handle for remote auth. NULL rows are profile-only. */
  email: string | null;
  /** Last offline transition timestamp for human presence. NULL until first disconnect. */
  last_seen_at: number | null;
  created_at: number;
  updated_at: number;
}

export interface UserPasswordCredentialRow {
  user_id: string;
  password_hash: string;
  /** JSON-encoded Argon2id parameter record selected by the env benchmark. */
  password_hash_params: string;
  password_updated_at: number;
}

export interface AuthSessionRow {
  id: string;
  user_id: string;
  token_hash: string;
  created_at: number;
  last_used_at: number;
  revoked_at: number | null;
  user_agent: string | null;
  ip_first_seen: string | null;
}

export interface WorkspaceRow {
  id: string;
  name: string;
  /** Internal current-address key. Present only on regular workspaces and
   * stripped from Workspace; agent_home is outside the namespace. */
  name_key: string | null;
  description: string | null;
  owner_agent_id: string | null;
  /**
   * Human governance owner (graded-permissions Layer A, v40). The live
   * "who owns this workspace" pointer; `null` only on rows created
   * before an owner is assigned (no such row exists post-migration on
   * the current install — the single local human owns every workspace).
   * Denormalized convenience + the anchor for the "exactly one owner"
   * invariant; the authoritative governance gate reads
   * `workspace_users.role`, not this column.
   */
  owner_user_id: string | null;
  dm_channel_id?: string | null;
  dm_channel_ids?: string[];
  archived_at: number | null;
  created_at: number;
  updated_at: number;
}

export interface WorkspaceMemberRow {
  workspace_id: string;
  agent_id: string;
  joined_at: number;
}

/**
 * Human membership row for `workspace_users` (v25). Mirrors
 * `WorkspaceMemberRow` but for users instead of agents. Unlike
 * `workspace_members`, this table accepts ALL workspace kinds —
 * including `agent_home` and DM workspaces — because humans have no
 * `owner_agent_id` equivalent that would let them access a workspace
 * implicitly.
 */
export interface WorkspaceUserMemberRow {
  workspace_id: string;
  user_id: string;
  joined_at: number;
  /**
   * Workspace governance role (graded-permissions Layer A, v40).
   * Defaults to `member` for new rows; the workspace creator / migrated
   * single local human is `owner`. This column is the authoritative
   * source for `canGovernWorkspace`.
   */
  role: WorkspaceRole;
}

/**
 * Workspace governance role (graded-permissions Layer A, v40). Two
 * orthogonal authority axes exist: this human role axis (coarse,
 * governance-scoped) and the per-(workspace, agent) capability axis
 * (fine, agent-scoped, landed in Layer A cut 2). They never collapse.
 *
 * - `owner`  — existence + human-roster control (archive/delete the
 *              workspace, add/remove human members, grant admin).
 *              Exactly one per workspace.
 * - `admin`  — day-to-day governance (create/delete channels, add/remove
 *              agent members, edit settings incl. rename, edit agent
 *              capabilities). Fully modeled in backend; NOT surfaced in
 *              the UI yet (amendment 4).
 * - `member` — view / post only (subject to conversation membership);
 *              the default role for a newly added human.
 */
export type WorkspaceRole = 'owner' | 'admin' | 'member';

/**
 * The agent-capability axis (graded-permissions Layer A cut 2, v41). The
 * 11 coarse capability buckets (doc §2.2) that gate what an agent may do
 * in a given workspace. CANONICAL definition — the migration CHECK
 * constraint, the DTO zod enum, and the backend route registry all derive
 * from this one list, so adding a bucket is a single-edit change.
 *
 * Orthogonal to {@link WorkspaceRole}: the human axis is coarse and
 * governance-scoped (who may *edit* capabilities); this axis is fine and
 * per-(workspace, agent) (what an agent may *do*). The two never collapse
 * — see docs/graded-permissions-v1.md §2 and the §6.3 orthogonality test.
 */
export const AGENT_CAPABILITIES = [
  'read_messages',
  'send_messages',
  'read_tasks',
  'create_tasks',
  'write_tasks',
  'read_attachments',
  'upload_attachments',
  'create_threads',
  'create_channels',
  'manage_conversation_members',
  'manage_schedules',
] as const;

export type AgentCapability = (typeof AGENT_CAPABILITIES)[number];

/**
 * One granted capability for an (agent, workspace) pair (v41). PRESENCE =
 * GRANTED — there is no `granted` boolean; a missing row means the agent
 * lacks that capability in that workspace. The composite (workspace_id,
 * agent_id) is a FK onto `workspace_members` ON DELETE CASCADE, so caps
 * auto-delete when the agent leaves the workspace (no orphan caps,
 * doc §2.4).
 *
 * NOTE on shape: this table keys on `agent_id` (NOT a polymorphic
 * `actor_kind + actor_id`) by design — capabilities are inherently the
 * agent-only axis; humans use {@link WorkspaceRole}, not capabilities, so
 * there is no user variant of this row. This is the justified exception to
 * the actor-isomorphism anti-pattern (architecture-principles §6), not a
 * violation of it.
 */
export interface WorkspaceAgentCapabilityRow {
  workspace_id: string;
  agent_id: string;
  capability: AgentCapability;
}

export interface ConversationRow {
  id: string;
  /** The workspace arm of the placement arc (v71+): NOT NULL exactly when
   *  `placement_kind = 'workspace'`, NULL for every other placement. Read
   *  surfaces must therefore not INNER JOIN `workspaces` to reach a
   *  conversation — a `direct_pair` row has no workspace at all. */
  workspace_id: string | null;
  /** The two arms of a `direct_pair` placement (v73+): NOT NULL exactly when
   *  `placement_kind = 'direct_pair'`, NULL for every other placement, and
   *  stored in canonical order (`a < b` by `(kind, id)`, which also rules out a
   *  self-pair). HISTORICAL PRINCIPAL COORDINATES — no foreign key: the DM must
   *  outlive the deletion of the principal on the other side. Validated at birth
   *  by trigger (the id must exist in the table its `_kind` names) and immutable
   *  afterwards, so a lookup that MISSES means "deleted", never "wrong id". */
  endpoint_a_kind: ConversationEndpointKind | null;
  endpoint_a_id: string | null;
  endpoint_b_kind: ConversationEndpointKind | null;
  endpoint_b_id: string | null;
  /** WHEN the endpoint's principal was hard-deleted (v73+), stamped by the
   *  `BEFORE DELETE` trigger on `agents` / `users`. A DENORMALIZED TIMESTAMP,
   *  never the authority for "gone": existence is derived by lookup, and every
   *  read surface asks `deleted_at IS NOT NULL OR the lookup misses`, so a
   *  delete path that failed to stamp cannot restore the survivor's write
   *  access. Null for every non-`direct_pair` placement. */
  endpoint_a_deleted_at: number | null;
  endpoint_b_deleted_at: number | null;
  /** The `direct_pair` identity as ONE scalar, DERIVED by the DB from the four
   *  endpoint coordinates (GENERATED VIRTUAL, v74+): readable, never writable,
   *  and NOT NULL exactly when `placement_kind = 'direct_pair'`. Its bytes are a
   *  contract (P0-f's task-board key is `'pair:' || pair_key`) and are defined
   *  ONLY in the v74 migration — derive nothing from a handle or a title, and do
   *  not re-implement the format in TypeScript: look a pair up by its four
   *  ordered coordinates instead. */
  pair_key: string | null;
  /** Authoritative placement axis (v69+). */
  placement_kind: ConversationPlacementKind;
  /** Authoritative roster axis (v69+). */
  roster_policy: ConversationRosterPolicy;
  title: string;
  /** The TITLE AS AN ADDRESS (v72+, #task:2016): `NFC(foldCase(NFC(trim(title))))`,
   *  unique per workspace among workspace placements, and NOT NULL exactly when
   *  `placement_kind = 'workspace'` — a DirectPair holds no title
   *  address, so a resolver that forgot its placement predicate still cannot
   *  reach one.
   *
   *  A PLAIN column, not GENERATED: SQLite has neither Unicode case folding nor
   *  NFC, so the fold cannot be a SQL expression and the UNIQUE index proves only
   *  that STORED KEYS are unique. The claim users are given ("two channels here
   *  cannot show the same name") holds because
   *  `canonicalizeStoredConversationTitle` is the ONE canonicalizer, the writer
   *  only accepts an unforgeable title+key pair, and
   *  `auditConversationTitleAddresses` can detect a half-state at runtime. Never
   *  derive this in a query, and never write it from anything but that pair. */
  title_key: string | null;
  status: ConversationStatus;
  spawned_by_agent_id: string | null;
  closed_at: number | null;
  close_summary: string | null;
  pinned_at: number | null;
  archived_at: number | null;
  latest_message_seq: number;
  created_at: number;
  updated_at: number;
  /** `private` (default, membership-gated) | `public` (workspace
   *  members may browse; interaction still requires membership). Always
   *  `private` when `roster_policy = 'fixed'`. */
  visibility: ConversationVisibility;
}

export interface ConversationAgentRow {
  conversation_id: string;
  agent_id: string;
  joined_at: number;
}

/**
 * Current runtime slot authority. Scope is discriminated structurally:
 * `conversation_id IS NULL` is Agent scope; non-NULL is Conversation scope.
 * Process fields are observations, while `cli_session_id` is durable resume
 * state and survives ordinary process exit.
 */
export interface AgentRuntimeSlotRow {
  id: number;
  agent_id: string;
  conversation_id: string | null;
  generation: number;
  claimed_by_daemon_instance_id: string | null;
  observed_status: ConversationAgentStatus;
  observed_pid: number | null;
  cli_session_id: string | null;
}

export type AgentRuntimeEventKind =
  | 'slot_migrated'
  | 'generation_claimed'
  | 'projection_updated'
  | 'generation_finalized';

/** Append-only history; current authority is always AgentRuntimeSlotRow. */
export interface AgentRuntimeEventRow {
  id: number;
  slot_id: number;
  agent_id: string;
  conversation_id: string | null;
  generation: number;
  event_kind: AgentRuntimeEventKind;
  daemon_instance_id: string | null;
  observed_status: ConversationAgentStatus;
  observed_pid: number | null;
  cli_session_id: string | null;
  occurred_at: number;
}

export type AgentRuntimeControlReason = 'session_cleared';

/**
 * Append-only user control audit for the Agent/Runtime aggregate. Historical
 * agent and actor coordinates deliberately have no FK, and the shape has no
 * conversation coordinate: an AgentSlot focus is presentation, not ownership.
 */
export interface AgentRuntimeControlEventRow {
  id: number;
  agent_id: string;
  generation: number;
  actor_user_id: string;
  reason: AgentRuntimeControlReason;
  was_running: 0 | 1;
  previous_cli_session_id_prefix: string | null;
  occurred_at: number;
}

/** Human membership row for `conversation_users` (v25). */
export interface ConversationUserMemberRow {
  conversation_id: string;
  user_id: string;
  joined_at: number;
}

export interface MessageRow {
  client_message_id?: string | null;
  client_request_json?: string | null;
  id: string;
  conversation_id: string;
  from_kind: FromKind;
  from_agent_id: string | null;
  from_user_id: string | null;
  content: string;
  metadata: string | null;
  seq: number;
  created_at: number;
  /** Send-time author snapshot (v63, #task:1791). Survives the actor's hard
   *  delete — `author_id` has NO FK, so it is retained when
   *  `from_agent_id`/`from_user_id` are nulled by ON DELETE SET NULL. It is the
   *  stable, non-collapsing aggregation key + the evidence that lets a resolver
   *  render a "deleted" author. NULL for system rows and for legacy rows whose
   *  actor was deleted before the snapshot existed (→ 'unresolved'). See
   *  MessageAuthorDto. */
  author_id: string | null;
  author_name: string | null;
  author_display_name: string | null;
  /** The source message this row is a mention CC copy OF (v71+, §5); NULL for
   *  every ordinary message. Deliberately carries NO foreign key: the copy is an
   *  independent fact inside the recipient's inbox, so deleting the source
   *  conversation must not delete a notification someone already received.
   *  `UNIQUE(cc_source_message_id, conversation_id)` (partial) is the
   *  "exactly one copy per inbox" proof — the copy itself is written inside the
   *  source message's own transaction, so it is a safety belt against a second
   *  write path, not the correctness mechanism. */
  cc_source_message_id: string | null;
  /**
   * Reply-cluster thread anchor (v43). NULL = this message is a root /
   * main-line message; non-NULL = a reply in the thread rooted at that
   * message id (same conversation, root itself has thread_root_id NULL).
   * See docs/thread-reply-cluster-v1.md.
   */
  thread_root_id: string | null;
}

/**
 * First-class task aggregate (v60) — a task is NO LONGER a `messages` row with
 * `task_*` annotations. `id` is the identity everywhere (REST/events/mutations);
 * `task_number` is the per-workspace human/CLI resolver. Migrated tasks reuse
 * their old anchor message id as `id` (that id WAS the task identity), so
 * historical references and system-event `task_message_id` links carry forward
 * unchanged. See docs and the v60 migration.
 *
 * Invariants (enforced by DB CHECKs, see migration v60): status ∈ enum;
 * task_number > 0; at most one assignee column set; creator_kind
 * pairs exactly with its id column ('system' = both id columns null, for tasks
 * whose creating agent/user was since deleted — a supported lifecycle);
 * last-editor is all-null or a complete (kind, id, edited_at) triple;
 * `closed_at IS NOT NULL ⟺ status = 'closed'`.
 * `source_message_id` / `created_event_message_id` are RAW ids (NOT foreign
 * keys) — retained verbatim if the referenced message is deleted so navigation
 * resolves to `unavailable` rather than silently reclassifying.
 */
export interface TaskRow {
  id: string;
  workspace_id: string;
  task_number: number;
  conversation_id: string;
  thread_root_id: string | null;
  title: string;
  content: string;
  status: TaskStatus;
  assignee_agent_id: string | null;
  assignee_user_id: string | null;
  creator_kind: 'agent' | 'user' | 'system';
  creator_agent_id: string | null;
  creator_user_id: string | null;
  source_message_id: string | null;
  created_event_message_id: string | null;
  last_editor_kind: 'agent' | 'user' | null;
  last_editor_id: string | null;
  last_edited_at: number | null;
  created_at: number;
  closed_at: number | null;
}

/** Entry path that first made a principal a follower. Immutable per row —
 *  a later mention never rewrites a `manual` follow's provenance. Single
 *  authority: the follower repo and the message-domain ports both name THIS
 *  type instead of re-declaring the union (#task:1981). */
export type ThreadFollowerSource = 'reply' | 'mention' | 'manual' | 'target_author';

/**
 * Thread delivery/notification subscription (v43) — NOT membership or
 * permission. `follower ⊆ parent channel member` is enforced at query
 * time, not by this table. `source` records the initial entry path
 * (immutable; later entries don't overwrite). See thread-reply-cluster-v1 §5/§9.
 */
export interface ThreadFollowerRow {
  thread_root_id: string;
  conversation_id: string;
  principal_kind: 'human' | 'agent';
  principal_id: string;
  followed_at: number;
  source: ThreadFollowerSource;
}

/**
 * Per-stream freshness head (v43) — the optimistic-lock row for
 * message-stale CAS. A stream is (conversation_id, thread_root_id|NULL);
 * `latest_seq` is the global messages.seq of the stream's newest message
 * and doubles as the CAS token + newer-messages boundary. See §6.4.
 */
export interface ConversationStreamHeadRow {
  conversation_id: string;
  thread_root_id: string | null;
  latest_seq: number;
}

export interface ConversationReadRow {
  user_id: string;
  conversation_id: string;
  /** Per-stream read cursor (#task:1226): NULL = mainline, non-NULL = that
   *  reply-cluster. Uniqueness enforced by two partial-unique indexes
   *  (main / thread), mirroring conversation_stream_heads. */
  thread_root_id: string | null;
  last_read_seq: number;
  read_at: number;
}

/** Per-(viewer, channel) attention mode (#task:1227 / #task:1870,
 *  docs/activity-lifecycle-v1.md §9.1). One of the Attention bounded context's
 *  TWO owned write-side aggregates (the other is
 *  {@link WorkspaceAttentionPreferenceRow}). `mode` 'all' = ambient on,
 *  'mentions' = ambient off with the positional residue still hinted, 'mute' =
 *  ambient off and the positional hint suppressed too (directed @me is never
 *  suppressed); absence of a row = default `'all'`. The value set's wire
 *  authority is `CHANNEL_ATTENTION_MODES` (api/attention.ts) and its storage
 *  authority is this table's CHECK — the union below is kept literal only to keep
 *  db/ a leaf module. Regular channel only (a DM's unread is directed-grade and a
 *  thread's ambient is follow-driven, so neither carries this preference). */
export interface ChannelAttentionPreferenceRow {
  user_id: string;
  conversation_id: string;
  mode: 'all' | 'mentions' | 'mute';
  updated_at: number;
}

/** Per-(viewer, workspace) attention mute SUPPRESSOR (#task:1870) — the second
 *  Attention-owned write-side aggregate. Deliberately isomorphic to
 *  {@link ChannelAttentionPreferenceRow} (own table, own PK, own `updated_at`)
 *  rather than a column on `workspace_users`: it is a viewer's ATTENTION
 *  decision, not membership state, and hanging it off membership would tie its
 *  lifecycle to a row that is also written by governance.
 *
 *  A boolean, not a third tier: it sits ABOVE the channel tier and suppresses,
 *  so un-muting the workspace restores every channel's own stored mode
 *  unchanged. Absence of a row = not muted ⇒ no migration and no backfill.
 *  Regular workspace only (enforced by trigger, mirroring the channel table's
 *  `channel_only` guard). */
export interface WorkspaceAttentionPreferenceRow {
  user_id: string;
  workspace_id: string;
  /** SQLite INTEGER 0/1. */
  muted: number;
  updated_at: number;
}

export interface AgentInboxRow {
  id: number;
  conversation_id: string;
  agent_id: string;
  message_id: string;
  delivered_at: number | null;
  created_at: number;
}

export interface SessionEventRow {
  id: number;
  conversation_id: string;
  agent_id: string | null;
  kind: ConversationEventKind;
  payload: string | null;
  created_at: number;
}

export interface SavedMessageRow {
  id: string;
  user_id: string | null;
  message_id: string;
  conversation_id: string;
  saved_at: number;
}

export const StorageKindSchema = z.enum(['local', 's3', 'oss']);
export type StorageKind = z.infer<typeof StorageKindSchema>;

export const AttachmentCreatorKindSchema = z.enum(['user', 'agent']);
export type AttachmentCreatorKind = z.infer<typeof AttachmentCreatorKindSchema>;

/**
 * One uploaded blob, scoped to a channel-like conversation (channel/dm_channel)
 * for access control. Reply-cluster uploads share their parent channel —
 * `source_conversation_id` keeps the lineage.
 *
 * No de-duplication: every upload creates a new row + new blob, so each
 * filename/source/creator is preserved cleanly.
 *
 * Content is mutable in place via the creator's attachment-update path
 * (`AttachmentService.update`): `storage_key` + `size` + `content_revision`
 * are repointed atomically to a freshly written physical blob (pointer-swap),
 * while `id`, `filename`, and `content_type` stay locked for the row's life.
 * `storage_key` is therefore a pure PHYSICAL pointer (it moves on every update),
 * NOT a stable public handle — the stable handle is `id`.
 */
export interface AttachmentRow {
  id: string;
  /** channel/dm_channel that owns this blob (access boundary). */
  channel_id: string;
  /** channel-like conversation (channel/dm_channel) that originated the upload (audit / lineage). */
  source_conversation_id: string | null;
  filename: string;
  size: number;
  content_type: string;
  storage: StorageKind;
  storage_key: string;
  /** Monotonic per-attachment freshness token, bumped on every content update.
   *  A cache-bust / renderer-generation signal ONLY — NOT a version system
   *  (no history, no rollback) and NOT an HTTP validator. Starts at 1. */
  content_revision: number;
  created_by_kind: AttachmentCreatorKind;
  created_by_id: string;
  created_at: number;
}

export const NotificationKindSchema = z.enum([
  'mention_external',
  /** The registered human viewer was @-mentioned in a conversation. */
  'mention_self',
  /** An agent failed to start before its process was even spawned
   *  (e.g. runtime binary not on PATH). Inserted by ConversationManager. */
  'agent_start_failed',
]);
export type NotificationKind = z.infer<typeof NotificationKindSchema>;

export interface NotificationRow {
  id: string;
  kind: NotificationKind;
  /** Reserved for multi-user. v1 always writes NULL. */
  user_id: string | null;
  /** The agent the notification is *about* (e.g. mentioned agent). */
  agent_id: string | null;
  /** Sidebar/workspace scope where the notification lives. */
  workspace_id: string | null;
  /** Conversation where the notification's message lives. */
  conversation_id: string | null;
  /** The CC message itself. */
  message_id: string | null;
  /** Origin pointers (where the user actually typed the mention). */
  source_workspace_id: string | null;
  source_conversation_id: string | null;
  source_message_id: string | null;
  /** JSON; kind-specific extra fields. */
  payload: string | null;
  read_at: number | null;
  created_at: number;
}
