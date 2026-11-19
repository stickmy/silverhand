// Viewer-local projection removal contract at CHANNEL scope (#task:1689).
//
// BUG CLASS this closes: "a third party removed the viewer from channel X" is a
// §12 visibility-loss domain fact with the same fan-out problem the workspace
// arm already solved (#task:1411/#task:1412) — one fact, many independent
// viewer-local React-Query caches, and a hand-maintained cleanup list whose
// omissions are SILENT. Until now the renderer had NO channel-level convergence
// at all: self-leave settled its own caches inside the mutation's `onSuccess`,
// so a removal performed by someone else left the message window, roster,
// attention preference, unread badge, Saved rows, task rows and Activity rows of
// a channel the viewer can no longer see sitting in the cache until an unrelated
// refetch.
//
// The workspace file (`features/workspaces/viewer-removal-contract.ts`) is the
// template; the two differences are deliberate:
//
//   * SCOPE KEY. Layer 1 sweeps by the id in key position 1 (`['channels', cid,
//     …]`, `['conversations', cid, …]`, `['conversation', cid, …]`,
//     `['conversation-members', cid]`, `['channel-detail', cid]`). Matching the
//     POSITION, not an allow-list of roots, is what makes this
//     form-by-construction: a channel-keyed cache added under a new root prefix
//     tomorrow is covered the moment it exists. Ids are unique, so position 1
//     cannot collide across families.
//
//   * MODE decides NAVIGATION and, within the channel-keyed layer, DISCARD vs
//     RE-READ — never WHICH caches are covered. A private channel drops out of
//     discovery entirely (visibility loss) and its caches are destroyed; a public
//     one stays readable with `is_member: false` (membership loss), so the SAME
//     predicate covers it but is graded by the ACTIVITY axis — what the viewer is
//     looking at is revalidated in place, everything unobserved is dropped. The
//     viewer is still on that pane, and emptying it reads as a full reload (shiki
//     2026-07-28). Neither mode consults a hand-maintained list of "which
//     channel-keyed slices are member-only" (thread-followers / threads-list /
//     attachments / run-states …), the exact drift-prone inventory this contract
//     exists to abolish: the member-only ones are gated off for a non-member, so
//     they are the unobserved ones and get dropped without being named.
//
// WHAT A CLEANUP MUST PRODUCE (the rule every registry entry below is judged by):
// the cache as a REFETCH would return it. That is what decides patch vs discard
// vs retain per read model — a list the server membership-filters is patched to
// the filtered shape; a ranked/capped payload or a single-object read the server
// would refuse is discarded; and a read model the server does NOT filter by
// membership is RETAINED, because deleting rows there would diverge from the
// server and be resurrected on the next refetch (see `notifications` in the
// classification below).
import type { QueryClient, QueryKey } from "@tanstack/react-query";
import type { ListActivityDto, SavedMessageDto, TaskSummaryDto } from "@silverhand/shared";
import {
  activityKey,
  activityHeldChannelIds,
  removeConversationFromActivity,
} from "@/features/attention/hooks";
import { removeChannelFromSaved, savedHeldChannelIds, savedKey } from "@/features/messages/saved";
import { removeChannelFromSearch, searchHeldChannelIds, searchKey } from "@/features/search/hooks";
import { removeChannelFromTasks, tasksHeldChannelIds } from "@/features/tasks/hooks/hooks";
import { boardColumnKey, boardFilterKey } from "@/features/tasks/model/board-query";
import { DEFAULT_TASK_FILTERS } from "@/features/tasks/model/task-filters";
import {
  messageUnreadHeldChannelIds,
  messageUnreadSummaryKey,
  removeConversationFromMessageUnread,
} from "@/features/channels/message-unread";
import {
  messageKey,
  messagesHeldChannelIds,
  removeChannelFromMessages,
} from "@/features/messages/hooks";
import { ApiError } from "@/lib/api";
import { channelWorkspaceOf } from "./channel-workspace-index";
import { isMemberChannel, type MembershipCarrier } from "./membership";

/** Why the viewer stopped being a member — decides NAVIGATION and, for the
 *  channel-keyed layer, DISCARD vs RE-READ (see {@link handleViewerChannelRemoved}).
 *  `visibility-loss`: the channel left discovery (private removal, or deleted) —
 *  the primary pane must leave it and its caches are destroyed.
 *  `membership-loss`: a public channel the viewer can still browse — the pane
 *  stays and collapses to the Join affordance, so what is still observed is
 *  revalidated in place and only the unobserved caches are dropped. */
export type ChannelLossMode = "visibility-loss" | "membership-loss";

/** The one fact every cleanup consumes. `workspaceId` is required because two
 *  projections are keyed by the WORKSPACE and carry the channel inside the
 *  payload (the `by_conversation` unread map and the global summary). */
export interface ChannelRemovalTarget {
  readonly workspaceId: string;
  readonly channelId: string;
  readonly mode: ChannelLossMode;
}

/** Structural marker: a value that belongs to exactly one conversation.
 *  Renderer-local BY DESIGN — TS is structural, so `ActivityItemDto` satisfies it
 *  with no shared `extends`. The field is REQUIRED: an optional
 *  `conversation_id?` would NOT match, so the cleanup guard would silently stop
 *  firing (the type-test pins the current DTO as attributed). */
export type ConversationAttributed = { conversation_id: string };

/** The one payload shape whose attribution TS can PROVE: a list of attributed
 *  items. Drives the mandatory-cleanup constraint in {@link itemScopedList}. */
export type ConversationScopedListData = { items: ConversationAttributed[] };

/** A row that names both axes this contract needs: which workspace it belongs to
 *  (so a NON-selected workspace's held channels are enumerable) and which channel
 *  (so the cleanup can filter). Required fields on purpose — see
 *  {@link attributedRowFamily}. */
export type ChannelAttributedRow =
  | { workspace_id: string; channel_id: string }
  /** #2045: row DTOs fold their attribution into a discriminated `place`. The
   *  proof accepts the arm-attributed shape, and enumerators derive the workspace
   *  by narrowing the workspace arm and reading `place.conversation_id` as the
   *  channel coordinate (a DirectPair row belongs to no workspace and is never
   *  held by one). The workspace arm's discriminant differs by DTO — `TaskPlace`
   *  spells it `workspace` (#task:2041), `SavedPlace` spells it `workspace_channel`
   *  (#task:2055) — so both literals are accepted here; each carries the same two
   *  axes (`workspace_id` + `conversation_id`). */
  | {
      place:
        | { kind: "workspace"; conversation_id: string; workspace_id: string }
        | { kind: "workspace_channel"; conversation_id: string; workspace_id: string }
        | { kind: "direct_pair"; conversation_id: string };
    };

/** How a viewer-local cache carries a channel — each settles differently.
 *  Exhaustive: a new member must also gain a dispatch branch in
 *  {@link handleViewerChannelRemoved} or the `never` check fails to compile. */
export type ChannelProjectionForm =
  /** key = `[…, cid, …]` with the id in position 1 → layer-1 predicate sweep. */
  | "conversation-keyed"
  /** global/workspace key, data = `{ …: Record<cid, …> }` → delete map entry. */
  | "record-keyed-map"
  /** global key, data = `{ items: ConversationAttributed[] }` → filter items. */
  | "item-scoped-list"
  /** MANY keys under one root, data = rows carrying `workspace_id`+`channel_id`
   *  → filter rows in every loaded variant (Saved, task board). */
  | "attributed-row-family"
  /** many keys under one root whose payload REFERENCES channels but cannot be
   *  patched into refetch shape (ranked/capped result sets, single-object reads)
   *  → discard the referencing query. */
  | "opaque-reference-family";

type ChannelCleanupFn = (qc: QueryClient, target: ChannelRemovalTarget) => void;

/** Which channels of ONE workspace a read model still holds state for. Required
 *  on every entry: it is what lets a removal in a NON-selected workspace converge
 *  without navigating there (see {@link heldChannelIdsInWorkspace}), so a new
 *  registry entry cannot silently opt out of that path. */
type ChannelHeldIdsFn = (qc: QueryClient, workspaceId: string) => Iterable<string>;

/** A viewer-local read model that needs targeted (non-sweep) removal cleanup. */
export interface RegisteredChannelReadModel {
  readonly id: string;
  readonly form: ChannelProjectionForm;
  readonly key: QueryKey;
  /** EVERY query-key root this entry's cleanup writes. Joined against
   *  {@link VIEWER_LOCAL_CHANNEL_KEY_CLASSIFICATION} by the smoke test, so a
   *  cleanup that reaches across roots (unread patches `['workspaces', …]` too)
   *  cannot hide that reach. */
  readonly roots: readonly string[];
  readonly cleanup: ChannelCleanupFn;
  readonly heldChannelIds: ChannelHeldIdsFn;
}

interface ChannelReadModelEntry {
  id: string;
  key: QueryKey;
  roots: readonly string[];
  cleanup: ChannelCleanupFn;
  heldChannelIds: ChannelHeldIdsFn;
}

/**
 * Register an ITEM-SCOPED list read model. Attribution is proven at the type
 * level exactly as in the workspace contract: the param is
 * `TData extends ConversationScopedListData ? Entry : never`, so `TData` must be
 * passed explicitly AND the DTO must genuinely carry `conversation_id`; a
 * non-attributed DTO — or an omitted type param — collapses the param to `never`.
 * `cleanup` is a required field, so registration without a cleanup cannot
 * compile.
 */
export function itemScopedList<TData>(
  entry: TData extends ConversationScopedListData ? ChannelReadModelEntry : never,
): RegisteredChannelReadModel {
  return { ...(entry as ChannelReadModelEntry), form: "item-scoped-list" };
}

/**
 * Register a Record-keyed map read model (`{ by_conversation: Record<cid, …> }`
 * and its rollups). A `Record<string, …>` is structurally any object, so
 * attribution is NOT type-detectable — the form is explicit and `cleanup` is
 * required by signature.
 */
export function recordKeyedMap(entry: ChannelReadModelEntry): RegisteredChannelReadModel {
  return { ...entry, form: "record-keyed-map" };
}

/**
 * Register a family of caches whose ROWS carry both attribution axes. Same
 * conditional-`never` proof as {@link itemScopedList}, one axis stronger: the row
 * type must carry `workspace_id` AND `channel_id`, which is exactly what
 * {@link heldChannelIdsInWorkspace} needs to enumerate a non-selected workspace's
 * held channels. A row type missing either field collapses the param to `never`.
 */
export function attributedRowFamily<TData>(
  entry: TData extends readonly ChannelAttributedRow[] ? ChannelReadModelEntry : never,
): RegisteredChannelReadModel {
  return { ...(entry as ChannelReadModelEntry), form: "attributed-row-family" };
}

/**
 * Register a family whose payload references channels but cannot be patched into
 * refetch shape. No type proof is possible (the attribution is nested inside a
 * discriminated union), so this form is a DECLARATION that the cleanup discards
 * rather than patches — the falsifier asserts the referencing query is gone, not
 * trimmed.
 */
export function opaqueReferenceFamily(entry: ChannelReadModelEntry): RegisteredChannelReadModel {
  return { ...entry, form: "opaque-reference-family" };
}

/**
 * THE registry: every viewer-local read model whose channel attribution lives in
 * the PAYLOAD, so no key sweep can reach it. Channel-KEYED caches are absent by
 * design — layer 1 covers them by form.
 */
export const VIEWER_LOCAL_CHANNEL_READ_MODELS: readonly RegisteredChannelReadModel[] = [
  itemScopedList<ListActivityDto>({
    id: "activity",
    key: activityKey(),
    roots: ["activity"],
    cleanup: (qc, t) => removeConversationFromActivity(qc, t.channelId),
    heldChannelIds: activityHeldChannelIds,
  }),
  recordKeyedMap({
    id: "message-unread",
    key: messageUnreadSummaryKey,
    // Settles THREE places at once: the workspace `by_conversation` map + its
    // `total`, and the global summary's `by_workspace[wsId]`. The workspace map
    // is keyed by the WORKSPACE, so the layer-1 channel sweep can never reach it
    // — hence the second root.
    roots: ["message-unread", "workspaces"],
    cleanup: (qc, t) => removeConversationFromMessageUnread(qc, t.workspaceId, t.channelId),
    heldChannelIds: messageUnreadHeldChannelIds,
  }),
  attributedRowFamily<SavedMessageDto[]>({
    id: "saved",
    key: savedKey,
    roots: ["saved"],
    cleanup: (qc, t) => removeChannelFromSaved(qc, t.channelId),
    heldChannelIds: savedHeldChannelIds,
  }),
  attributedRowFamily<TaskSummaryDto[]>({
    id: "tasks",
    // The Todo column under the default filters — the key every board mount
    // creates, and the row-carrying face this entry patches. A representative
    // key must name a variant that EXISTS: it used to be `tasksKey({})`, whose
    // fetch-all list was retired with #task:1892.
    key: boardColumnKey("todo", boardFilterKey(DEFAULT_TASK_FILTERS)),
    // One entry, three roots: the column pages are patched, the two
    // single-object task reads are discarded, and all three are membership-gated
    // by the SAME server predicate (board / task read → `EXISTS
    // conversation_users`), so splitting them would only let one drift. The two
    // board aggregates share the `tasks` root and are invalidated by the same
    // cleanup — they carry no rows to patch.
    roots: ["tasks", "task-detail", "task-by-number"],
    cleanup: (qc, t) => removeChannelFromTasks(qc, t.channelId),
    heldChannelIds: tasksHeldChannelIds,
  }),
  opaqueReferenceFamily({
    id: "search",
    key: searchKey,
    roots: ["search"],
    cleanup: (qc, t) => removeChannelFromSearch(qc, t.channelId),
    heldChannelIds: searchHeldChannelIds,
  }),
  opaqueReferenceFamily({
    id: "messages",
    key: messageKey("any"),
    roots: ["messages"],
    // Keyed by MESSAGE id, so no key sweep reaches it, but the payload's
    // `conversation_id` is the parent channel — discarded, because a single-object
    // read has no membership-filtered shape to patch to.
    cleanup: (qc, t) => removeChannelFromMessages(qc, t.channelId),
    heldChannelIds: messagesHeldChannelIds,
  }),
];

/** What this contract does about one query-key root. `swept` = layer 1 reaches it
 *  by key position; `cleaned` = a registry entry writes it; `refetch-authoritative`
 *  = the server re-read settles it (and, for the channel lists, is what DETECTS
 *  the loss); `retained` = deliberately left alone with a reason; `exempt` = the
 *  root carries no channel attribution at all. */
export type ChannelKeyVerdict =
  | "swept"
  | "cleaned"
  | "refetch-authoritative"
  | "retained"
  | "exempt";

export interface ChannelKeyClassification {
  readonly verdicts: readonly ChannelKeyVerdict[];
  readonly reason: string;
}

/**
 * Classification of EVERY React-Query key ROOT used anywhere under
 * `features/**` — not just the three read-model files the first cut fenced. The
 * source scan in the smoke test derives the root set from the source tree and
 * asserts this table matches it exactly, in BOTH directions: a new root goes RED
 * (no "unknown ⇒ exempt" default, which would silently empty the net), and a root
 * that disappears goes RED too, so the table cannot rot into fiction.
 *
 * Root granularity is deliberate and is the honest unit: a root is what a key's
 * first segment declares, and `workspaces` proves why it must carry a SET of
 * verdicts — its channel lists are refetch-authoritative, its unread map is
 * patched by a registry entry, and its member/capability slices carry no
 * channel attribution. Recording one verdict per root would force a lie.
 *
 * §12.1#9 RESIDUAL: the scan recognizes the three key shapes the codebase
 * actually writes (a `*Key` factory, a multi-line factory, and an inline
 * `queryKey:` literal). A new key written in a shape the scanner does not
 * recognize — a computed root, a key assembled from a variable — is NOT caught
 * by this net. It is a completeness net over the known shapes, not a
 * query-definition chokepoint; the renderer has no such chokepoint today.
 */
export const VIEWER_LOCAL_CHANNEL_KEY_CLASSIFICATION: Record<string, ChannelKeyClassification> = {
  "account-deletion": {
    verdicts: ["exempt"],
    reason:
      "the viewer's own account-deletion preflight (blocking workspaces and agents); no conversation axis",
  },
  activity: {
    verdicts: ["cleaned"],
    reason: "global list; every item carries conversation_id → registry `activity`",
  },
  "agent-audit-chats": {
    verdicts: ["retained"],
    reason:
      "['agent-audit-chats', agentId] — the owner-audit Chats list (#task:2036). Scoped by AGENT ownership + the agent's participation resolved server-side, NOT by the viewer's channel membership: a viewer losing a channel does not change the owner's audit ACL, so there is nothing to clean, and a local delete would diverge from an authoritative refetch. The key carries no channel_id to sweep by",
  },
  agents: {
    verdicts: ["exempt"],
    reason:
      "keyed by agent identity (['agents'], ['agents', id], ['agents', id, 'workspace']); an agent's presence in a channel lives under ['channels', cid, 'agents'] → swept",
  },
  "archived-channels": {
    verdicts: ["refetch-authoritative"],
    reason:
      "cross-workspace archived restore index, server membership-filtered; already invalidated on workspace_channels_changed / snapshot_required and re-read",
  },
  attention: {
    verdicts: ["refetch-authoritative"],
    reason:
      "['attention','badges'] roll-up + ['attention','modes', wsId] tier batch (#task:1870) — both are server membership-filtered (each JOINs conversation_users) and neither is patchable here: the badge is an absolute per-workspace number this client cannot recompute, and the batch's rows are the server's tier resolution. Settled by invalidate-and-re-read on workspace_channels_changed / snapshot_required — a membership change deliberately emits NO attention_badge_updated frame",
  },
  "avatar-design": {
    verdicts: ["exempt"],
    reason: "saved avatar-maker design per principal; no conversation axis",
  },
  "backend-info": { verdicts: ["exempt"], reason: "backend build info; no conversation axis" },
  "server-environment": {
    verdicts: ["exempt"],
    reason: "connection mode and server features; no conversation axis",
  },
  "channel-detail": { verdicts: ["swept"], reason: "['channel-detail', cid] → position 1" },
  channels: {
    verdicts: ["swept"],
    reason:
      "['channels', cid, …] — messages, clusters, live gaps, thread roots/followers, threads-list, attachments, channel agents → position 1",
  },
  conversation: {
    verdicts: ["swept"],
    reason: "['conversation', cid, …] — per-stream unread → position 1",
  },
  "conversation-members": {
    verdicts: ["swept"],
    reason: "['conversation-members', cid] → position 1",
  },
  "conversation-ref": { verdicts: ["swept"], reason: "['conversation-ref', cid] → position 1" },
  "conversation-latest": {
    verdicts: ["retained"],
    reason:
      "previews for the Recent sidebar, whose rows come from the channel and direct-pair lists; an entry for a lost conversation is never rendered and the next snapshot drops it",
  },
  conversations: {
    verdicts: ["swept"],
    reason: "['conversations', cid, 'agent-activity', agentId] → position 1",
  },
  "direct-pairs": {
    verdicts: ["exempt"],
    reason:
      "['direct-pairs'] — the viewer's `direct_pair` list (#task:2035). Not a channel projection in either direction: the rows have no workspace and no channel_id to filter by, and a pair can never BE the lost channel — its roster is its two endpoints, so there is no `conversation_users` row to lose and no channel list that could detect one. The viewer-scoped `direct_pairs_changed` invalidation is this list's only settler",
  },
  favorites: {
    verdicts: ["exempt"],
    reason:
      "favorites are conversation rows keyed by parent workspace — channel loss does not settle that parent-scoped projection here",
  },
  host: { verdicts: ["exempt"], reason: "computer-scoped host record; no conversation axis" },
  "host-proxy-config": {
    verdicts: ["exempt"],
    reason: "computer-scoped host network config; no conversation axis",
  },
  "host-resources": {
    verdicts: ["exempt"],
    reason: "computer-scoped host resource sampling; no conversation axis",
  },
  "runtime-models": {
    verdicts: ["exempt"],
    reason: "computer/runtime-scoped model suggestions; no conversation axis",
  },
  "host-runtime-catalog": {
    verdicts: ["exempt"],
    reason: "computer-scoped runtime inventory; no conversation axis",
  },
  "host-status": {
    verdicts: ["exempt"],
    reason: "computer-scoped host liveness; no conversation axis",
  },
  hosts: {
    verdicts: ["exempt"],
    reason: "computer-scoped host list; no conversation axis",
  },
  "inline-find": { verdicts: ["swept"], reason: "['inline-find', cid, …] → position 1" },
  me: { verdicts: ["exempt"], reason: "viewer identity + capabilities; no conversation axis" },
  "me-email": { verdicts: ["exempt"], reason: "viewer's own email; no conversation axis" },
  "message-reactions": {
    verdicts: ["retained"],
    reason:
      "keyed by message id, and the PAYLOAD is a reaction aggregate that names no conversation — this client has no message→channel carrier for it. The only readers are the swept channel projections, so entries are unreachable after the loss and expire by gc. NOT the same reason as `messages`, whose payload does carry the attribution",
  },
  "message-unread": {
    verdicts: ["cleaned"],
    reason: "global summary; patched by registry `message-unread`",
  },
  messages: {
    verdicts: ["cleaned"],
    reason:
      "single-message cache keyed by message id; its MessageDto.conversation_id is the PARENT channel-like conversation (a thread is identified by thread_root_id, not a separate conversation), so the entry is channel-attributable and is discarded by registry `messages`",
  },
  "name-availability": { verdicts: ["exempt"], reason: "transient naming probe" },
  notifications: {
    verdicts: ["retained"],
    reason:
      "RETAINED ON PURPOSE: notification-repo filters by recipient only, NOT by conversation membership, so a refetch still returns these rows — a local delete would diverge from the server and be resurrected. Whether losing a channel should clear its notifications is a backend domain decision, not a viewer-local cleanup",
  },
  "password-status": {
    verdicts: ["exempt"],
    reason: "viewer's own credential state; no conversation axis",
  },
  presence: { verdicts: ["exempt"], reason: "per-user presence; no conversation axis" },
  "related-users": {
    verdicts: ["exempt"],
    reason:
      "users sharing a workspace with the viewer; refetched on workspace membership change, no channel axis",
  },
  "profile-message-counts": {
    verdicts: ["exempt"],
    reason:
      "public per-subject annual counts, independent of source conversation ACL; channel membership loss does not change this aggregate",
  },
  saved: { verdicts: ["cleaned"], reason: "rows carry channel_id → registry `saved`" },
  search: {
    verdicts: ["cleaned"],
    reason: "ranked/capped result sets referencing the channel are discarded → registry `search`",
  },
  "task-by-number": {
    verdicts: ["cleaned"],
    reason: "single-object task read → discarded by registry `tasks`",
  },
  "task-detail": {
    verdicts: ["cleaned"],
    reason: "single-object task read → discarded by registry `tasks`",
  },
  "task-history": {
    verdicts: ["refetch-authoritative"],
    reason:
      "one task's events, rendered only inside that task's detail (whose read is discarded by registry `tasks`); the server gates it by the same task read and every mount re-reads it (default staleTime)",
  },
  tasks: { verdicts: ["cleaned"], reason: "rows carry channel_id → registry `tasks`" },
  users: { verdicts: ["exempt"], reason: "user directory; no conversation axis" },
  workspaces: {
    verdicts: ["cleaned", "refetch-authoritative", "exempt"],
    reason:
      "one root, three answers: the message-unread map (['workspaces', ws, 'message-unread']) is patched by registry `message-unread`; the channel + archived channel lists are refetch-authoritative and are what DETECTS the loss; members / capabilities / workspace list carry no channel attribution",
  },
};

/** Predicate: is this cache keyed by the lost channel? Matches the id in key
 *  POSITION 1 — `['channels', cid, …]` (messages / clusters / gaps / thread
 *  roots+followers / agents / threads-list / attachments), `['conversations',
 *  cid, …]` (agent run states + activity feed), `['conversation', cid, …]`
 *  (per-stream unread, attention preference), `['conversation-members', cid]`
 *  and `['channel-detail', cid]`. Position-based on purpose: no root allow-list
 *  to keep in sync. */
function isConversationKeyed(queryKey: unknown, channelId: string): boolean {
  return Array.isArray(queryKey) && queryKey[1] === channelId;
}

function assertNever(x: never): never {
  throw new Error(`unhandled viewer-local channel projection form: ${String(x)}`);
}

/**
 * Settle EVERY viewer-local projection of ONE channel the viewer stopped being a
 * member of — the single chokepoint both removal triggers (the selected
 * workspace's list detector and the foreign-workspace reconcile) funnel through.
 *
 * Layer 1: one predicate sweep over every channel-keyed cache (by form, no
 * enumeration), DISCARD or RE-READ per `mode` (below). Layer 2: dispatch each
 * registered payload-attributed read model by `form`, with an exhaustive `never`
 * so a new form cannot skip a branch — mode-independent, because those read
 * models are membership-filtered by the server in BOTH modes, so a row of this
 * channel is a row no refetch would return. The workspace channel list /
 * archived list are NOT touched here: they are refetch-authoritative and are in
 * fact what DETECTS this fact ({@link resolveChannelMembershipLosses}).
 *
 * LAYER 1 FORKS ON `mode`, and the fork is the whole point (shiki 2026-07-28:
 * leaving a public channel "整个会话 UI 会刷新一次"). ONE predicate throughout —
 * the fork is DISCARD vs RE-READ, and for `membership-loss` it is graded by the
 * cache's ACTIVITY AXIS, never by a keep-list of "which slices are member-only"
 * (that inventory is what this contract exists to abolish; an omission in it is
 * silent):
 *   - `visibility-loss` → `removeQueries({ predicate })`. The channel is gone from
 *     discovery and the pane is navigated away, so nothing on screen loses its
 *     data; a private channel must not stay resolvable on any read path (§12 read
 *     authority).
 *   - `membership-loss` → the same predicate, split in two:
 *     `invalidateQueries({ predicate, type: 'active' })` keeps the data of what
 *     the viewer is LOOKING AT and revalidates it in the background — destroying
 *     the mainline window / cluster windows / detail would be a visible reload of
 *     that pane (and would drop the loaded pagination) in exchange for nothing,
 *     since those reads are public-aware and return the same rows;
 *     `removeQueries({ predicate, type: 'inactive' })` drops everything nobody is
 *     observing, so no member-only slice (roster, agents/run-state, threads /
 *     attachments panels, per-viewer unread) survives as cached data.
 *
 * WHY THE SPLIT IS SOUND AND NOT A DISGUISED KEEP-LIST: for a non-member every
 * member-only hook is `enabled`-gated off by `gate.allowMemberEffects`, so at the
 * moment this sweep runs those queries have no enabled observer — they are
 * `inactive` and land in the `remove` arm; what stays `active` is precisely what a
 * non-member still renders. The order matters and it is the caller's: both removal
 * triggers run this from an EFFECT, i.e. after the commit in which the membership
 * carrier already flipped to `is_member: false`, so the activity axis read here is
 * the post-flip one (`Query.isActive()` is `observers.some(o => o.options.enabled
 * !== false)`, and v5 pushes new observer options during render). Calling this
 * from a mutation's `onSuccess` in the same tick as the flip would read the
 * PRE-flip observer set and send a member-only request that is bound to 403.
 *
 * NAMED RESIDUAL (bounded, deliberately not engineered around): a channel of a
 * NON-selected workspace shown in the side pane is reconciled from a WS frame
 * rather than from a commit, so a member-only query of it can still be active for
 * one tick and take a single 403 before its gate closes. It keeps its previous
 * `data` in an error state, renders nothing (the gate is already shut), and is
 * collected on unmount. The alternative — a global "403 ⇒ drop the query" rule —
 * was rejected: it would paper over gate-wiring defects with a cross-layer
 * side effect (@Cindy / @Alice, 2026-07-28).
 *
 * Idempotent: every cleanup is present-only, so a repeat call writes nothing.
 */
export function handleViewerChannelRemoved(qc: QueryClient, target: ChannelRemovalTarget): void {
  const predicate = (query: { queryKey: unknown }) =>
    isConversationKeyed(query.queryKey, target.channelId);
  if (target.mode === "visibility-loss") {
    qc.removeQueries({ predicate });
  } else {
    void qc.invalidateQueries({ predicate, type: "active" });
    qc.removeQueries({ predicate, type: "inactive" });
  }
  for (const rm of VIEWER_LOCAL_CHANNEL_READ_MODELS) {
    switch (rm.form) {
      case "conversation-keyed":
        // Covered by the layer-1 sweep; no registered entry uses this form
        // today, but the branch keeps the dispatch exhaustive + documents it.
        break;
      case "record-keyed-map":
      case "item-scoped-list":
      case "attributed-row-family":
      case "opaque-reference-family":
        rm.cleanup(qc, target);
        break;
      default:
        assertNever(rm.form);
    }
  }
}

/**
 * Which channels of ONE workspace the viewer still holds cleanable state for.
 *
 * This is the gate of the NON-SELECTED workspace path (#task:1689 blocker 2): a
 * §12 domain fact must not wait for navigation, but a workspace the viewer is not
 * looking at has no mounted channel lists, so there is no "previously observed
 * member set" to diff. The answer is the other direction — the caches themselves
 * say which channels they still describe, and only those are worth an
 * authoritative re-read.
 *
 * TWO sources, because the two layers carry attribution differently:
 *   - payload-attributed read models answer for themselves (their rows carry
 *     `workspace_id`);
 *   - layer-1 CHANNEL-KEYED caches carry no workspace anywhere, so they are
 *     attributed through the immutable identity index
 *     ({@link channelWorkspaceOf}). Leaving them out — as the first cut did, on
 *     the theory that channel-keyed caches belong to the workspace the viewer has
 *     open — was wrong twice over: React Query keeps `['channels', cid, …]`,
 *     `['conversation-members', cid]`, `['channel-detail', cid]` and run states
 *     after the viewer walks B → A, and a workspace holding ONLY those would fail
 *     the `observed.length > 0` gate and never even be re-read. Coming back to B
 *     could not repair it either: the first converged tick there only establishes
 *     a baseline, and the removed channel is in neither list to be diffed.
 */
export function heldChannelIdsInWorkspace(qc: QueryClient, workspaceId: string): Set<string> {
  const ids = new Set<string>();
  for (const rm of VIEWER_LOCAL_CHANNEL_READ_MODELS) {
    for (const id of rm.heldChannelIds(qc, workspaceId)) ids.add(id);
  }
  for (const id of channelKeyedHeldChannelIds(qc, workspaceId)) ids.add(id);
  return ids;
}

/**
 * The layer-1 half of {@link heldChannelIdsInWorkspace}: every id in key position
 * 1 that the identity index attributes to this workspace.
 *
 * Only the IMMUTABLE binding is consulted — never a cached list's membership — so
 * an old entry cannot make this claim anything but "that key describes a channel
 * of workspace W". Whether the viewer may still see it is decided afterwards, by
 * the fresh authoritative read. The same lookup is what keeps a key whose position
 * 1 is NOT a channel id (`['workspaces', wsId, …]`, `['messages', mid]`) out: an
 * unbound id is skipped, not guessed at.
 *
 * §12.1#9 RESIDUAL: a channel-keyed cache for a channel this client has never
 * seen a workspace-carrying DTO for is not enumerable here. Every carrier the
 * renderer has TODAY states the workspace — the selected workspace's two lists,
 * channel-detail, the cross-workspace archived index, the one shared
 * conversation-ref read point (`?side` panes and inline `#conv:` chips), and a
 * notification's own `(workspace_id, conversation_id)` pair — and the two
 * carrier-side enumerations in `channel-workspace-index.ts` are what keep a new
 * ref producer from landing without the write-through. A REFUSED carrier leaves
 * nothing behind either: a `?side` ref that 403s resolves to `unauthorized`, and
 * that descriptor renders `SideUnresolvedPlaceholder` instead of mounting
 * `ChannelView`, so the path that cannot learn the workspace is the same path
 * that writes no channel-keyed cache. What remains open is a FUTURE carrier of a
 * different shape that yields a bare channel id: that is the named gap,
 * deliberately not covered by a second store — an unattributed cache is skipped
 * and expires by gc, and a CONFLICTING attribution is dropped outright rather
 * than being trusted.
 */
export function channelKeyedHeldChannelIds(qc: QueryClient, workspaceId: string): Set<string> {
  const ids = new Set<string>();
  for (const query of qc.getQueryCache().getAll()) {
    const key = query.queryKey;
    if (!Array.isArray(key)) continue;
    const candidate = key[1];
    if (typeof candidate !== "string" || !candidate) continue;
    if (ids.has(candidate)) continue;
    if (channelWorkspaceOf(candidate) === workspaceId) ids.add(candidate);
  }
  return ids;
}

/** A channel row as the detection reads it: identity, membership carrier, and
 *  visibility (which decides the mode). Both the discovery-union list item and
 *  the archived list item satisfy it — since #task:1691 the archived listing
 *  returns the same `WorkspaceChannelListItem` shape, so the resolver no longer
 *  has to know WHICH list a row came from to read membership off it. */
export interface ChannelMembershipRow extends MembershipCarrier {
  id: string;
  visibility?: "public" | "private";
}

export interface ChannelMembershipLoss {
  readonly channelId: string;
  readonly mode: ChannelLossMode;
}

/**
 * Detect "the viewer stopped being a member of channel X" from the SETTLED
 * authoritative lists — §12.1#6: read-path validation is the authority, the
 * write path is belt-and-suspenders. There is no dedicated backend event for
 * this fact and none is needed (§12.2#3, one fact → one handler): the removed
 * viewer still holds the WORKSPACE scope, so `workspace_channels_changed`
 * already invalidates both lists, and their refetch is the observation.
 *
 * `observed` is the set of channel ids the loss is evaluated FOR — the previous
 * settled tick's authoritative member set (selected workspace), or the set of
 * channels whose viewer-local state we still hold (foreign workspace). A loss is
 * reported only for an id in that set, and:
 *   - a row that is STILL an authoritative member → no loss;
 *   - a row whose membership is `unknown` (carrier without `is_member`) → NO
 *     loss. Fail-open here is deliberate and is the opposite of the gate's
 *     fail-closed rule: the gate withholds an affordance, this handler DESTROYS
 *     caches and moves the primary pane, so an unresolved carrier must never
 *     trigger it;
 *   - an ARCHIVED row the viewer still belongs to → no loss (§12.2#2, archive is
 *     not removal — this is why the archived list is an input at all);
 *   - a surviving PUBLIC row with `is_member: false` → `membership-loss` (still
 *     readable, so the pane stays);
 *   - a surviving row that is PRIVATE (or carries no visibility) with
 *     `is_member: false` → `visibility-loss`. A private non-member row should not
 *     be listable at all, so this is the fail-safe direction: if one ever is, the
 *     viewer cannot read it and the pane must leave;
 *   - absent from BOTH lists → `visibility-loss` (private removal, or the
 *     channel was deleted — the same convergence either way).
 *
 * Pure, so the transition table is unit-tested without React or a nav stack.
 */
export function resolveChannelMembershipLosses(input: {
  observed: Iterable<string>;
  active: readonly ChannelMembershipRow[];
  archived: readonly ChannelMembershipRow[];
}): ChannelMembershipLoss[] {
  const rows = new Map<string, ChannelMembershipRow>();
  for (const row of input.archived) rows.set(row.id, row);
  for (const row of input.active) rows.set(row.id, row);
  const losses: ChannelMembershipLoss[] = [];
  for (const channelId of input.observed) {
    const row = rows.get(channelId);
    if (!row) {
      losses.push({ channelId, mode: "visibility-loss" });
      continue;
    }
    if (isMemberChannel(row)) continue; // still a member
    if (typeof row.is_member !== "boolean") continue; // unresolved → never fire
    losses.push({
      channelId,
      mode: row.visibility === "public" ? "membership-loss" : "visibility-loss",
    });
  }
  return losses;
}

/**
 * Did the authoritative read fail because the viewer has NO visibility into that
 * workspace, rather than because the read did not happen?
 *
 * Both list routes are gated by `canViewWorkspace`: a non-member gets **403** and
 * a deleted/missing workspace **404** — so "a third party removed me from
 * workspace B" does not surface as "the channel is absent from B's two tables",
 * it surfaces as BOTH reads failing. Treating that like a transport blip
 * (observe nothing → change nothing) would leave every projection of B —
 * unread badge, Activity, Saved and task rows — describing channels the viewer
 * can no longer see, with no later observation to settle them: the workspace
 * scope is truncated the moment the membership edge is deleted, so no further
 * `workspace_channels_changed` arrives either. 403 and 404 mean the same thing
 * here (no visibility into B) and need no separate mapping.
 *
 * Everything else — 5xx, timeout, `backend_unreachable`, 401 — is NOT this fact.
 * A failed read must never be evidence of a removal (@Alice's second contract
 * point, and the same discipline as {@link isConvergedListTick}: only a
 * successful read is an observation).
 */
export function isWorkspaceVisibilityDenied(error: unknown): boolean {
  return (
    error instanceof ApiError &&
    error.kind === "http" &&
    (error.status === 403 || error.status === 404)
  );
}

/**
 * The losses implied by losing visibility into a whole workspace: every channel
 * of it the viewer still holds state for. No per-channel resolution is possible
 * (or needed) — the lists cannot be read at all, and every channel in them is
 * equally unreachable, which makes this branch both stronger and simpler than the
 * row-by-row one.
 *
 * `visibility-loss` is the honest mode: those channels are gone from discovery.
 * It changes no cache scope (see header), and the foreign path navigates on
 * neither mode.
 *
 * Deliberately NOT a workspace-scope convergence: the workspace row itself, its
 * membership caches and the sidebar entry are a different domain fact with a
 * different (and larger) handler. This clears exactly the channel-scope
 * projections this contract owns.
 */
export function resolveWorkspaceVisibilityLosses(held: Iterable<string>): ChannelMembershipLoss[] {
  return [...held].map((channelId) => ({ channelId, mode: "visibility-loss" as const }));
}

/** The authoritative member set for one settled tick — the `observed` input of
 *  the NEXT tick. Archived channels the viewer still belongs to are included, so
 *  archiving alone never reads as a loss. */
export function observedMemberChannelIds(
  active: readonly ChannelMembershipRow[],
  archived: readonly ChannelMembershipRow[],
): Set<string> {
  const ids = new Set<string>();
  for (const row of active) if (isMemberChannel(row)) ids.add(row.id);
  for (const row of archived) if (isMemberChannel(row)) ids.add(row.id);
  return ids;
}

/** One tick of the selected workspace's two authoritative lists.
 *
 *  `converged` must mean "BOTH lists have settled AND neither is waiting to
 *  refetch". React Query keeps `status: 'success'` while a background refetch is
 *  in flight, so `status`-only would accept the SPLIT-BRAIN tick — active list
 *  already refetched, archived list not yet — in which a channel that just moved
 *  between the two tables is momentarily absent from both, which this contract
 *  would read as a visibility loss and act on by destroying caches and moving the
 *  primary pane (#task:1689 blocker 1). */
export interface ChannelListsTick {
  readonly workspaceId: string | null;
  readonly converged: boolean;
  readonly active: readonly ChannelMembershipRow[];
  readonly archived: readonly ChannelMembershipRow[];
}

/** The authoritative member set carried between ticks. */
export interface ChannelRemovalBaseline {
  readonly workspaceId: string;
  readonly ids: Set<string>;
}

/**
 * The convergence predicate itself, pure and separate from React Query's hook
 * result, because it is the FIX for blocker 1 and therefore has to be falsifiable
 * on its own. Per list, two independent facts:
 *   - `settled` = `status === 'success' && fetchStatus === 'idle'` — a list that
 *     is loading, refetching, or errored into an empty default is not an
 *     observation;
 *   - `awaitingRefetch` = the query is invalidated and its refetch has not landed
 *     (React Query's `isInvalidated`, which the hook result does not surface). An
 *     invalidated list still reports `success` with its PREVIOUS rows, so this is
 *     the half that excludes reading a pre-invalidation snapshot as the new truth.
 *
 * Both lists must clear both facts. The split-brain window is exactly the state
 * where one of them has and the other has not.
 */
export function isConvergedListTick(input: {
  activeSettled: boolean;
  archivedSettled: boolean;
  activeAwaitingRefetch: boolean;
  archivedAwaitingRefetch: boolean;
}): boolean {
  return (
    input.activeSettled &&
    input.archivedSettled &&
    !input.activeAwaitingRefetch &&
    !input.archivedAwaitingRefetch
  );
}

export interface ChannelRemovalDecision {
  readonly baseline: ChannelRemovalBaseline | null;
  readonly losses: readonly ChannelMembershipLoss[];
}

/**
 * The whole trigger decision as a PURE function: previous baseline + this tick →
 * losses to converge + the baseline to carry forward. Extracted from the effect
 * so the acceptance sequence (baseline → both lists invalidated → active settles
 * first → archived settles carrying the row) is a unit test rather than a UI
 * test.
 *
 * Two rules, both of them the fix rather than plumbing:
 *   - an UNCONVERGED tick is inert: no losses AND no baseline update. Updating
 *     the baseline mid-refetch would be worse than firing a false loss — it would
 *     silently record a half-refetched member set as authoritative and make the
 *     NEXT tick's diff wrong in either direction;
 *   - a workspace switch only re-baselines (a loss is only meaningful against a
 *     baseline observed in the SAME workspace).
 */
export function reduceChannelRemoval(
  prev: ChannelRemovalBaseline | null,
  tick: ChannelListsTick,
): ChannelRemovalDecision {
  if (!tick.workspaceId || !tick.converged) return { baseline: prev, losses: [] };
  const baseline: ChannelRemovalBaseline = {
    workspaceId: tick.workspaceId,
    ids: observedMemberChannelIds(tick.active, tick.archived),
  };
  if (!prev || prev.workspaceId !== tick.workspaceId) return { baseline, losses: [] };
  return {
    baseline,
    losses: resolveChannelMembershipLosses({
      observed: prev.ids,
      active: tick.active,
      archived: tick.archived,
    }),
  };
}
