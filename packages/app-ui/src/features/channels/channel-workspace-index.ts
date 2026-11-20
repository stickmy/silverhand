// Channel → workspace IDENTITY index (#task:1689 blocker 2).
//
// WHY THIS EXISTS. The channel-removal contract has to answer "which channels OF
// WORKSPACE B does this client still hold state for" for a workspace the viewer is
// NOT looking at. Payload-attributed read models answer it themselves (their rows
// carry `workspace_id`), but the layer-1 CHANNEL-KEYED caches — `['channels', cid,
// …]`, `['conversation-members', cid]`, `['channel-detail', cid]`, run states,
// inline-find — carry the channel id in the key and no workspace anywhere. After
// the viewer walks B → A those caches stay in React Query, and without an
// attribution source the foreign-workspace trigger cannot even tell they describe
// B, so a removal there would leave them until garbage collection.
//
// WHY IT IS SOUND. `conversations.workspace_id` is IMMUTABLE: no code path
// UPDATEs it (the only conversation updates are `archived_at`, `title`,
// `visibility`, `closed_at/status`) — a channel is never moved between
// workspaces. So a binding learned once is a permanent identity fact, and reading
// it back later cannot be stale in the way a cached LIST can.
//
// WHAT IT MUST NEVER DO. Answer visibility or membership. This index says only
// "channel X belongs to workspace W". Whether the viewer may still see X is
// decided exclusively by a fresh authoritative read. Keeping those two questions
// apart is what makes an old entry harmless.
//
// WRITE-THROUGH, never a second authority: every writer records the binding at
// the moment a DTO that carries it lands, so the index cannot disagree with the
// DTO it was derived from.
//
// IS AN UNATTRIBUTED CHANNEL-KEYED CACHE REACHABLE IN PRODUCTION? That is the
// question that decides whether a persistent index is needed. Every channel id
// the renderer can hand to a channel-keyed read arrives through one of the
// carriers in {@link CHANNEL_WORKSPACE_ATTRIBUTION_SOURCES}, each of which
// states the workspace: the primary pane resolves its carrier out of the
// selected workspace's two lists (or, when the id is in neither, out of
// channel-detail); conversation panes and every inline `#conv:<id>` chip resolve
// theirs through the ONE shared conversation-ref read point; and a notification
// jump records the pair the notification itself carries, which is what covers
// the reply-cluster branch that returns without fetching a ref at all.
//
// WHAT PINS THAT, AND WHAT IT DOES NOT PROVE. Two mechanical checks, both
// enumerated from the CARRIER side rather than from the recording side (a list
// drawn from the modules that already record can only ever confirm itself):
// `apiPaths.conversationRef` may appear in exactly one non-test module — the
// shared read point — and no other module may fetch the `['conversation-ref',
// …]` key (invalidate/remove only). Deleting a write-through, or adding a second
// ref producer without one, goes red. What they do NOT prove is that some future
// carrier of a different shape states a workspace at all; that residual is named
// below.
//
// A REFUSED ref is not a hole either: a 403 resolves to `unauthorized`, which
// renders the unresolved placeholder rather than mounting `ChannelView`, so the
// one path that cannot learn the workspace is also a path that writes no
// channel-keyed cache.
//
// §12.1#9 RESIDUAL: a FUTURE carrier that yields a bare channel id with no
// workspace anywhere (a new endpoint, a deep link parsed straight into a
// channel-keyed read). Its caches would be attributed to no workspace, so the
// foreign-workspace probe would skip them and they would expire by gc instead of
// being swept. The fail-safe direction — the index never guesses a workspace,
// and drops a binding entirely when two carriers disagree — is what keeps that
// residual harmless.

/** Every production carrier that records a channel → workspace binding, as
 *  `features/**`-relative module paths. This declaration is a
 *  don't-silently-shrink guard, NOT a completeness proof: the claim that no
 *  channel-keyed cache can be written for a channel whose workspace was never
 *  stated is pinned by the two carrier-side enumerations described above. */
export const CHANNEL_WORKSPACE_ATTRIBUTION_SOURCES = [
  // The selected workspace's two authoritative lists + channel-detail + the
  // cross-workspace archived index.
  "workspaces/hooks.ts",
  // THE conversation-ref read point: conversation panes and inline `#conv:` chips.
  "conversations/conversation-ref-query.ts",
  // Notification jumps, including the reply-cluster branch that never fetches a
  // ref — the notification's own `(workspace_id, conversation_id)` is the carrier.
  "attention/notifications.ts",
] as const;

const channelWorkspace = new Map<string, string>();

/** Channels whose carriers DISAGREED about the binding. Once here, the channel
 *  never gets an answer again — see {@link recordChannelWorkspace}. */
const conflicted = new Map<string, { first: string; second: string }>();

/** Record one immutable binding.
 *
 *  `workspaceId` is nullable because a conversation's workspace is a property of
 *  ONE placement arm (agent-scoped-session-v1 §4.1): a placement without one has
 *  no binding to record, and `null` here is a NO-OP rather than a caller-side
 *  narrowing. That keeps the null out of every carrier and lands it in the state
 *  this module already treats as harmless — an unattributed channel, whose caches
 *  expire by gc instead of being swept. Guessing a workspace for it is the one
 *  outcome that would be unsafe, and it is not available.
 *
 *  Agreement is a no-op; a DISAGREEMENT is a bug, and the one response that is
 *  not available is "keep the first value". This index fuses several carriers
 *  into a single load-bearing judgment that decides which channel-keyed caches a
 *  DESTRUCTIVE sweep may claim, so a wrong-but-confident binding would remove
 *  another workspace's state. Conflict therefore fails CLOSED: the binding is
 *  dropped and never re-established, `channelWorkspaceOf` answers `null` — which
 *  every caller already treats as "skip this id" — and the disagreement is left
 *  observable rather than swallowed. Being unable to attribute a cache is
 *  harmless (it expires by gc); attributing it wrongly is not. */
export function recordChannelWorkspace(channelId: string, workspaceId: string | null): void {
  if (!channelId || !workspaceId) return;
  if (conflicted.has(channelId)) return;
  const existing = channelWorkspace.get(channelId);
  if (existing === undefined) {
    channelWorkspace.set(channelId, workspaceId);
    return;
  }
  if (existing === workspaceId) return;
  channelWorkspace.delete(channelId);
  conflicted.set(channelId, { first: existing, second: workspaceId });
  console.error(
    `[channel-workspace-index] conflicting workspace for channel ${channelId}: ` +
      `${existing} then ${workspaceId} — binding dropped, cleanup will skip it`,
  );
}

/** Record every row of a workspace-scoped listing. `workspaceId` is the workspace
 *  the listing was fetched FOR, which is the binding for every row in it. */
export function recordChannelWorkspaceRows(
  rows: readonly { id: string }[],
  workspaceId: string,
): void {
  for (const row of rows) recordChannelWorkspace(row.id, workspaceId);
}

/** The workspace a channel belongs to, or null when this client has never seen a
 *  DTO that says — or when two carriers disagreed. Null is the honest answer and
 *  the fail-safe one: the caller skips the id rather than guessing a workspace
 *  for it. */
export function channelWorkspaceOf(channelId: string): string | null {
  return channelWorkspace.get(channelId) ?? null;
}

/** The conflicts observed this session, for diagnostics. Non-empty means some
 *  carrier is stating a workspace that another carrier contradicts, and those
 *  channels are permanently excluded from attribution-driven cleanup. */
export function channelWorkspaceConflicts(): readonly {
  channelId: string;
  first: string;
  second: string;
}[] {
  return [...conflicted].map(([channelId, pair]) => ({ channelId, ...pair }));
}

/** Drop everything. Session-scoped state, so this is for tests and for a sign-out
 *  that swaps the viewer's whole world. */
export function clearChannelWorkspaceIndex(): void {
  channelWorkspace.clear();
  conflicted.clear();
}
