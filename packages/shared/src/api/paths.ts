import { BACKEND_ADMISSION } from '../backend-admission.generated.js';
import { DAEMON_ADMISSION } from '../daemon-admission.generated.js';

export const apiPaths = {
  health: '/api/health',
  profileMessageCounts: (kind: 'user' | 'agent', id: string) =>
    `/api/profiles/${kind}/${encodeURIComponent(id)}/message-counts`,
  serverInfo: '/api/server/info',
  runtimes: '/api/runtimes',
  eventWebSocket: BACKEND_ADMISSION.eventWebSocketPath,
  presence: '/api/presence',

  // hosts
  hosts: '/api/hosts',
  /** D2 daemon-outbound WebSocket endpoint. The bundled desktop daemon
   *  presents `Authorization: Bearer <host_token>` during upgrade;
   *  this is host-token auth, not a user-session route. */
  hostConnect: DAEMON_ADMISSION.hostWebSocketPath,
  /** Resolve a reveal-once user-managed connect token to the server-side host
   *  id. The id is not present in the copied command; the daemon calls this
   *  with `Authorization: Bearer <connect-token>` before opening the outbound
   *  websocket, then still authenticates the websocket with the same token. */
  hostConnectTokenResolve: '/api/hosts/connect-token/resolve',
  hostClaimOrCreate: '/api/hosts/claim-or-create',
  /** Soft-delete a host the caller owns (Stage B.4). Sets
   *  `hosts.revoked_at` and bulk-revokes every active row in
   *  `host_credentials` for that host in one transaction. Idempotent:
   *  re-revoking an already-revoked host still sweeps any straggler
   *  credentials but does not move the original tombstone timestamp.
   *  No row cascade — agents/conversations bound to the host are
   *  preserved; runtime placement fails closed instead. Bearer must
   *  be a user session bearer. */
  hostRevoke: (id: string) => `/api/hosts/${id}/revoke`,
  /** Rotate the long-lived credential for a host the caller owns
   *  (Stage B.4). Server applies a 60-second grace to the prior
   *  credential and mints a fresh long_lived row, returning the new
   *  cleartext token. The token is consumed by Electron main only —
   *  `assertApiProxyPath` blocks this route from the generic renderer
   *  proxy, and the desktop IPC strips the token from any value that
   *  crosses back to the renderer. */
  hostRotateToken: (id: string) => `/api/hosts/${id}/rotate-token`,
  /** User-managed computer connect-token rotation. This is renderer-visible:
   *  the cleartext connect token is reveal-once in the response and must never
   *  be recoverable later. Desktop app-managed hosts continue using
   *  `hostRotateToken`, whose cleartext host_token belongs to Electron main. */
  hostRotateConnectToken: (id: string) => `/api/hosts/${id}/connect-token/rotate`,
  /** Disconnect the currently live daemon connection for a user-managed
   *  computer without rotating or revoking its credential. The same
   *  connection command remains valid and can reconnect later. */
  hostDisconnect: (id: string) => `/api/hosts/${id}/disconnect`,
  /** v36 (#task:808): rename a host the caller owns. Body =
   *  `UpdateHostInput { display_label }`. Only `display_label` is
   *  mutable today; `name` is the stable identifier set at claim time. */
  host: (id: string) => `/api/hosts/${id}`,
  /** Delete a user-managed computer together with everything rooted on it (its
   *  agents and its `regular` workspaces). Body = `HostDeleteInput`
   *  `{ cascade_dependents }`; without the flag a non-empty computer is refused
   *  with `has_dependents`. Unfinalized Add-computer drafts are naturally empty
   *  and use this same hard-delete path. */
  hostDelete: (id: string) => `/api/hosts/${id}`,
  /** Legacy alias for the Add-computer modal cancel path. */
  hostCancelPending: (id: string) => `/api/hosts/${id}`,
  /** What is persistently rooted on this computer: the agents placed on it and
   *  the `regular` workspaces whose files live on it. PERSISTENT OWNERSHIP —
   *  contrast `hostStatus`, which reports dynamic daemon/runtime availability. */
  hostResources: (id: string) => `/api/hosts/${id}/resources`,
  /** v36 (#task:808): dynamic host snapshot (OS / daemon version /
   *  registered agents + online state). Always HTTP 200 with tagged
   *  `availability` once owner ACL passes. 404 for missing / cross-user. */
  hostStatus: (id: string) => `/api/hosts/${id}/status`,
  /** Per-computer runtime availability catalog. The target daemon probes its
   *  own host-local resolver; server only dispatches and returns the result.
   *  `?force=1` bypasses the short resolver cache for manual refresh and
   *  create-time hard-block checks. */
  hostRuntimeCatalog: (id: string) => `/api/hosts/${id}/runtime-catalog`,
  /** Host-local proxy config. Server never stores proxy bytes; it dispatches
   *  get/set to the live daemon for the target computer. */
  hostProxyConfig: (id: string) => `/api/hosts/${id}/proxy-config`,

  // agents
  agents: '/api/agents',
  /** Create-form onBlur uniqueness probe. Static segment — MUST be
   *  registered before `agent(:id)` so it is not shadowed by the param route. */
  agentNameAvailability: '/api/agents/name-availability',
  agent: (id: string) => `/api/agents/${id}`,
  /** #task:811/#1190 — flat tree of every visible file/directory under the
   *  agent home dir by default, or under the agent's per-workspace L2 memory
   *  dir when `?workspace_id=` is supplied. The two-column
   *  AgentWorkspacePane/Browser drives left-side navigation off this list;
   *  `path` values are valid inputs to the read endpoint below. */
  agentWorkspaceTree: (id: string) => `/api/agents/${id}/workspace/tree`,
  /** #task:811/#1190 — read one file from the selected agent memory source
   *  (home/L1 by default, workspace L2 with `?workspace_id=`) by virtual path.
   *  The `path` query param is parsed segment-by-segment and rejects
   *  empty / `.` / `..` / dotfile / NUL / backslash segments; symlinks
   *  (intermediate or leaf) and any escape from the agent-home base are
   *  fail-closed (404 not_found, never a leaked path). Daemon-side preview cap;
   *  the response carries `truncated` so the UI can surface a banner. */
  agentWorkspaceFile: (id: string) => `/api/agents/${id}/workspace/file`,
  /** #task:1135 — read an agent's profile MCP config for the agent-pane
   *  "Profile MCP" section. Returns `{ raw: string | null }` (null when no
   *  `mcp.json`). Read requires `canEditAgent` (creator/self): raw mcp.json
   *  can carry secrets, so a non-editor viewer is 403'd even though
   *  `canViewAgent` would let them see the agent exists. The write path is
   *  agent-self-only via the internal CLI route — there is no public PUT. */
  agentProfileMcp: (id: string) => `/api/agents/${id}/profile/mcp`,
  /** Live-probe one of an agent's profile MCP servers (#task:1137). POST
   *  `{ server_name, include_tools? }`; returns a `McpProbeResult`. Same
   *  `canEditAgent` gate as the read mirror (a probe connects to the user's
   *  endpoint with their secrets) — a non-editor viewer is 403'd. The renderer
   *  (Test connection / List tools buttons) calls this. */
  agentProfileMcpProbe: (id: string) => `/api/agents/${id}/profile/mcp/probe`,
  agentRuntimeState: (id: string) => `/api/agents/${id}/runtime-state`,
  /** One-bit VIEWER-authorized availability (#task:2043): is this agent's host
   *  REACHABLE right now (outbound attachment connected + host not revoked +
   *  agent valid)? The reachability axis composed WITH the `agentPresence`
   *  deadline into Online/Away — an idle-reaped / not-yet-hydrated agent on a
   *  connected host is still available (reachable); it just reads Away once past
   *  its deadline. Same `canViewAgent` gate. Returns
   *  `{ availability: 'online' | 'offline' }` — two values only; a backend read
   *  fault is 5xx, and "cannot tell" is a renderer query-projection state, never
   *  a backend value. */
  agentAvailability: (id: string) => `/api/agents/${id}/availability`,
  /** VIEWER-authorized activity deadline (#task:2049), the axis composed WITH
   *  availability into Online/Away. Returns `{ away_at, server_now }`: `away_at`
   *  is the absolute epoch-ms instant a currently-available agent crosses to
   *  Away (or `null` = never active → Away, not Online); `server_now` is the
   *  backend clock at response time, so the renderer can calibrate the deadline
   *  against its own wall clock. Invalidated ONLY by `agent_presence_changed`. */
  agentPresence: (id: string) => `/api/agents/${id}/presence`,
  agentRuntime: (id: string) => `/api/agents/${id}/runtime`,
  agentStop: (id: string) => `/api/agents/${id}/stop`,
  /** "Message this agent" — a human opens (or reuses) their DirectPair with an
   *  AGENT-SCOPED agent and posts a message, in one call (#task:2036). Keyed by
   *  AGENT id, not conversation id, because a brand-new line has no conversation id
   *  yet: that is the whole reason this is its own door and not `channelMessages`.
   *
   *  LAZY ALLOCATE (@shiki's ruling): opening the composer and cancelling a draft
   *  hit the server not at all — only a real send does. The pair is get-or-create,
   *  so the ALLOCATION is idempotent under a double-submit (a second send lands in
   *  the same row); the POST is NOT — two sends post two messages. The renderer
   *  normally switches to `channelMessages` after the sidebar row appears.
   *
   *  Authorization is a two-step ladder, both BEFORE anything is created: anti-enum
   *  page visibility (`canViewAgent` → 404, hiding existence) then the named
   *  initiation gate (`canInitiateDirectMessageToAgent` → 403), whose verdict
   *  includes the target's session scope — only an agent-scoped agent takes a
   *  human-initiated line, a conversation-scoped agent's human entry stays its
   *  workspace/Agent Space. Every side-effect-free preflight (schema, content cap,
   *  reply-target rejection) also runs before the mint, so an unauthorized or
   *  malformed request leaves no pair and emits no `direct_pairs_changed`; only a
   *  downstream failure AFTER the mint may leave an announced empty line (the
   *  resolve-then-send window; `GET /api/direct-pairs` is the authority for what
   *  exists). Body is `SendMessageInput` (main-line only; `reply_to_message_id` is
   *  rejected). Returns `MessageSendAck` (201); `message.conversation_id` is the
   *  line the client then navigates to. */
  /** Authenticated human first-send; target must exist and differ from the viewer. */
  userDirectMessages: (id: string) => `/api/users/${id}/direct-messages`,
  agentDirectMessages: (id: string) => `/api/agents/${id}/direct-messages`,
  /** Owner-audit chats list (#task:2036 part ②): the conversations the agent is a
   *  party to that its OWNER may read in full, read-only. Enumerates the agent's
   *  workspace conversations and its `direct_pair` lines.
   *
   *  Owner-only by a two-step anti-enum ladder, matching `GET /api/agents/:id`:
   *  `canViewAgent` → 404 (hides existence from a non-viewer), then
   *  `canAuditAgentChats` → 403 (only the agent's creator, never a peer agent, a
   *  non-owner admin, or the agent itself). Returns `AgentAuditChatListResponse`.
   *  Each read leaves operator audit evidence in the request log. */
  agentChats: (id: string) => `/api/agents/${id}/chats`,
  /** Owner-audit transcript of ONE of the agent's chats (#task:2036 part ②):
   *  the full conversation (mainline + thread replies interleaved), windowed,
   *  read-only. Same window vocabulary as `channelMessages`
   *  (`limit`/`before_seq`/`after_seq`/`around_message_id`) but NO
   *  `thread_root_id` — the audit face returns the whole transcript, not a single
   *  stream. Returns `AgentAuditTranscriptResponse`, which carries NO per-viewer
   *  state: no reaction relation, no stream-head snapshot (the read advances no
   *  cursor), no subscription.
   *
   *  Same ladder as the list, plus the per-conversation gate
   *  (`canAuditAgentConversation`): `canViewAgent` → 404, then
   *  `canAuditAgentConversation` → 403 (owner ∧ the conversation exists ∧ the
   *  agent is a SUBJECT participant of it). A conversation
   *  the agent is not a party to is indistinguishable from a bogus id — no
   *  existence oracle. Each read leaves operator audit evidence. */
  agentChatTranscript: (id: string, conversationId: string) =>
    `/api/agents/${id}/chats/${conversationId}/messages`,
  /** Owner-audit read of ONE attachment's bytes inside one of the agent's chats
   *  (#task:2036 part ②). The transcript lists the messages; a DM whose content
   *  is an image or a file is only fully readable if the OWNER can fetch the blob
   *  too — the ruling is that the owner already sees everything the agent sees, so
   *  the audit face reads bytes read-only (never PATCH/DELETE, and this is NOT the
   *  channel Attachments panel).
   *
   *  Its own compound gate, NOT the ordinary `canViewAttachment` (which is
   *  membership-scoped and would still deny the owner on a third-party DM): the
   *  SAME anti-enum ladder as the transcript — `canViewAgent` → 404, then
   *  `canAuditAgentConversation` → 403 — AND the attachment must BELONG to that
   *  audited conversation. An attachment from another channel (or a bogus id) is
   *  the same 404, so the face reaches no blob outside the conversation it audits.
   *  Serves the raw bytes (same body shape as `GET /api/attachments/:id`). Each
   *  read leaves operator audit evidence. */
  agentChatAttachment: (id: string, conversationId: string, attachmentId: string) =>
    `/api/agents/${id}/chats/${conversationId}/attachments/${attachmentId}`,
  /** Public avatar fetch for any agent. Returns the blob bytes with a
   *  long immutable Cache-Control header (cache-busted via the
   *  `?v=updated_at` query param baked into `AgentDto.avatar_url`).
   *  Read gate matches `GET /api/agents/:id` (authed + agent exists);
   *  write gates (POST/DELETE) are creator-only via `canEditAgent`.
   *  Returns 404 when the agent has no avatar set. */
  agentAvatar: (id: string) => `/api/agents/${id}/avatar`,
  /** GET `AvatarDesignResponse`; same gate as the avatar write. */
  agentAvatarDesign: (id: string) => `/api/agents/${id}/avatar/design`,
  /** Per-agent schedule list for the user-facing Agent panel. Backed by
   *  `ScheduleService.list` — same data the agent sees via the MCP
   *  `list_schedules` tool, but reachable from the host UI without an
   *  agent-principal context. */
  agentSchedules: (id: string) => `/api/agents/${id}/schedules`,
  /** User-initiated cancel of one of the agent's schedules. Cancel is
   *  legal from `pending` or `firing`; the route validates ownership by
   *  passing both ids into `ScheduleService.cancel(scheduleId, agentId)`,
   *  which surfaces `not_owner` (403) when the schedule belongs to a
   *  different agent — preventing UI mis-drag from cancelling somebody
   *  else's row. */
  agentScheduleCancel: (agentId: string, scheduleId: string) =>
    `/api/agents/${agentId}/schedules/${scheduleId}/cancel`,

  // workspaces / conversations (new workspace -> channel -> thread model)
  workspaces: '/api/workspaces',
  /** Create-form onBlur uniqueness probe. Static segment — MUST be
   *  registered before `workspace(:id)` so it is not shadowed. */
  workspaceNameAvailability: '/api/workspaces/name-availability',
  workspace: (id: string) => `/api/workspaces/${id}`,
  /** Per-(viewer, workspace) attention MUTE suppressor (#task:1870). Owned by the
   *  Attention bounded context (served by the same route module as
   *  `conversationAttentionPreference`), URL-grouped here because the scope is the
   *  workspace. GET resolves the STORED boolean (default `false` when unset);
   *  nothing sits above the workspace scope, so this value is never "effective"
   *  in this context's sense — `effective` names only a channel tier that has
   *  already had this suppressor folded in;
   *  PUT sets it. It suppresses — never rewrites — each channel's own tier, so
   *  turning it off restores every channel's stored choice unchanged. Regular
   *  workspace only (agent-home workspaces 404). Human viewer-scoped. */
  workspaceAttentionPreference: (id: string) => `/api/workspaces/${id}/attention-preference`,
  workspaceMembers: (id: string) => `/api/workspaces/${id}/members`,
  workspaceMember: (id: string, agentId: string) => `/api/workspaces/${id}/members/${agentId}`,
  workspaceHumanMember: (id: string, userId: string) =>
    `/api/workspaces/${id}/members/users/${userId}`,
  workspaceHumanMemberRole: (id: string, userId: string) =>
    `/api/workspaces/${id}/members/users/${userId}/role`,
  /** Self-LEAVE a workspace (#task:1930). POST, empty body — the caller is
   *  ALWAYS the removed human, so there is no target on the wire to forge. This
   *  is the member/admin counterpart of `workspace` DELETE (owner-only): it ends
   *  the caller's own workspace membership and, in the same transaction, EVERY
   *  channel membership they hold in that workspace. The owner is refused
   *  (`owner_removal_unsupported`) — a workspace always has an owner, so the
   *  owner first transfers ownership or deletes the workspace.
   *  Agents the caller enrolled here are NOT touched (shiki 2026-08-09).
   *  Regular workspace only (agent-home / DM containers 404). */
  workspaceLeave: (id: string) => `/api/workspaces/${id}/leave`,
  /** Owner-only, one step: target human member becomes owner, caller becomes admin. */
  workspaceTransferOwnership: (id: string) => `/api/workspaces/${id}/transfer-ownership`,
  /** Agent capability set within a workspace (graded-permissions cut 2).
   *  GET reads the effective set (any workspace viewer); PUT replaces it
   *  (workspace admin, or a member who created the target agent — #task:1777). */
  workspaceAgentCapabilities: (id: string, agentId: string) =>
    `/api/workspaces/${id}/agents/${agentId}/capabilities`,
  workspaceChannels: (id: string) => `/api/workspaces/${id}/channels`,
  workspaceArchivedChannels: (id: string) => `/api/workspaces/${id}/channels/archived`,
  /** Global flat archived-channel list across every workspace the
   *  caller can view. Backs the "Archived" chat-tool page (filters:
   *  workspace, archived date). Per-workspace endpoint above is still
   *  used by the workspace-scoped query path. */
  archivedChannels: '/api/archived-channels',
  /** Per-user (viewer-scoped) list sort state mutations (#task:1147).
   *  Read state is carried inline on the workspace / channel list responses;
   *  these only WRITE preferences. All viewer-scoped. */
  sortMode: '/api/sort/mode',
  /** Complete server-ordered sidebar; mutations use sortOrder with sidebar group. */
  sidebarSort: '/api/sort/sidebar',
  sortOrder: '/api/sort/order',
  sortTouch: '/api/sort/touch',
  /** Personal ⭐ star toggle for a channel in the sidebar (#task:1209). POST =
   *  star, DELETE = unstar. Viewer-scoped, can-view gated — replaced the old
   *  admin-gated shared `/pin` route. */
  channelStar: (id: string) => `/api/channels/${id}/star`,
  /** The messages door for a conversation by id: POST sends, GET reads history
   *  (both verbs, one URL). The sole name for `/api/channels/:id/messages` —
   *  there is no `conversationChannel*` alias, so a caller cannot pick the wrong
   *  spelling of an identical URL. */
  channelMessages: (id: string) => `/api/channels/${id}/messages`,
  channelThreadFollowers: (id: string, rootMessageId: string) =>
    `/api/channels/${id}/threads/${rootMessageId}/followers`,
  channelThreadFollow: (id: string, rootMessageId: string) =>
    `/api/channels/${id}/threads/${rootMessageId}/follow`,
  channelAgents: (id: string) => `/api/channels/${id}/agents`,
  channelAgentRuntime: (id: string) => `/api/channels/${id}/agent-runtime`,
  channelAgentStart: (id: string, agentId: string) => `/api/channels/${id}/agents/${agentId}/start`,
  channelAgentStop: (id: string, agentId: string) => `/api/channels/${id}/agents/${agentId}/stop`,
  channelAgentClearSession: (id: string, agentId: string) =>
    `/api/channels/${id}/agents/${agentId}/clear-session`,
  /** AgentSlot session control. Unlike the channel route, this address has no
   * presentation target because the runtime is owned by the Agent aggregate. */
  agentClearSession: (agentId: string) => `/api/agents/${agentId}/clear-session`,
  channelAgentRunStates: (id: string) => `/api/channels/${id}/agent-run-states`,
  channelClose: (id: string) => `/api/channels/${id}/close`,
  channelStop: (id: string) => `/api/channels/${id}/stop`,
  channelResume: (id: string) => `/api/channels/${id}/resume`,
  channelArchive: (id: string) => `/api/channels/${id}/archive`,
  /** Self-join a PUBLIC channel. POST, empty body — the caller is
   *  ALWAYS the joined member, so there is no target on the wire to forge. The
   *  only non-member channel mutation; a member of a private / dm_channel /
   *  archived target is rejected. Returns the same `{channel, is_member:true}`
   *  envelope as channel detail so the client can backfill its cache. */
  channelJoin: (id: string) => `/api/channels/${id}/join`,
  /** Self-LEAVE a channel — the symmetric counterpart of `channelJoin`. A human
   *  member removes THEMSELVES (empty body, no forgeable target). Member-only
   *  (a non-member's only mutation is Join); an archived channel or the last
   *  human of a private channel is rejected with a typed reason. */
  channelLeave: (id: string) => `/api/channels/${id}/leave`,
  conversationChannel: (id: string) => `/api/channels/${id}`,
  conversationChannelAgents: (id: string) => `/api/channels/${id}/agents`,

  /** Identity + routing metadata for ANY top-level conversation by id. Backs
   *  cross-workspace secondary-pane
   *  resolution and the `#conv:<id>` sigil renderer. Returns
   *  `ConversationRefResponse`; does NOT include messages or per-user
   *  unread state. Permission: `canReadConversation` — the same
   *  public-aware read gate as channel detail + message history, i.e. a
   *  conversation member OR a workspace member browsing an ACTIVE PUBLIC
   *  channel. Workspace membership alone is NOT enough: a private channel
   *  / archived public channel / non-participant fixed-roster conversation in the
   *  viewer's own workspace all stay 403. */
  conversationRef: (id: string) => `/api/conversations/${id}/ref`,
  /** Per-conversation reply-cluster ("Threads") discovery list (#task:1074).
   *  Takes a top-level conversation id; thread clusters are message-derived and
   *  never have their own conversation row. */
  conversationThreads: (id: string) => `/api/conversations/${id}/threads`,
  /** Per-conversation per-viewer stream-unread read-model (#task:1226). The
   *  SINGLE client source of truth for thread / mainline "N new" + first-unread:
   *  the reply chip and the Threads list both read it, so the same fact never
   *  lives in two caches. Sparse (only threads with unread > 0). Refreshed
   *  ONLY by the viewer-scoped `unread_updated` WS signal — never inferred from
   *  message events (unread is server-scored). Human viewer-scoped. */
  conversationUnread: (id: string) => `/api/conversations/${id}/unread`,
  /** Every participating conversation's latest message summary (#task:2845). */
  conversationLatest: '/api/conversations/latest',
  /** Per-(viewer, channel) attention mode (#task:1227 / #task:1870,
   *  docs/activity-lifecycle-v1.md §9.3). **PUT only** — the stored mode's read
   *  face is `attentionModes`, which carries `mode` beside `effective_mode` for
   *  every channel in a workspace; a per-channel GET was retired as a second
   *  carrier of the same fact (#task:1876). PUT sets the channel
   *  `所有消息 / 仅提及 / 静音` control — one of the two surfaces that write
   *  attention state (the other is `workspaceAttentionPreference`), and neither
   *  ever touches delivery/follow. Regular channel only (dm_channel + thread 404 —
   *  a DM's unread is directed-grade, a thread's ambient is follow-driven). Human
   *  viewer-scoped. */
  conversationAttentionPreference: (id: string) => `/api/conversations/${id}/attention-preference`,
  /** The viewer's Activity list (#task:1227, docs/activity-lifecycle-v1.md §9.3)
   *  — the `listActivity` projection (directed mentions/tasks ∪ positional
   *  unread ∩ `enter()`), already deduped + salience-sorted. Read-only,
   *  human viewer-scoped. Optional `?workspace_id=` filters to one workspace
   *  (the data model is global; v1 UI is per-workspace). */
  activity: '/api/activity',
  /** The viewer's flat global Favorites — the EXCLUSIVE global STARRED list
   *  (#task:1252, re-scoped by #task:1888; docs/list-sort-v1.md): a starred
   *  CONVERSATION lives here and is moved OUT of its workspace's list. A read
   *  projection of `user_sort_entry.starred_at` across every conversation list
   *  (`channels:<ws_id>`) — the workspace rail cannot contribute, since a
   *  container has no star at all (#task:1899) — plus the viewer's
   *  `list_key='favorites'` ordering state. The authoritative display order comes
   *  from `viewer_sort` (recent → home last_accessed_at / manual → rank); raw
   *  `items` order is only a deterministic fallback, NOT pre-sorted by starred_at.
   *  Read-only, human viewer-scoped, global (no workspace filter). */
  favorites: '/api/favorites',
  /** The viewer's DIRECT MESSAGES list (#task:2035): every `direct_pair` the
   *  viewer is an endpoint of, with the counterpart resolved and the same unread
   *  roll-up a channel row shows. Returns `DirectPairListResponse`.
   *
   *  GLOBAL AND VIEWER-SCOPED, with no workspace parameter, because a
   *  `direct_pair` HAS no workspace coordinate (agent-scoped-session-v1 §4.1). That
   *  is why this is its own route and not an arm of `workspaceChannels`: there is no
   *  `:id` a DM belongs to, so a workspace-scoped response could only carry it by
   *  attaching it to whichever workspace the client asked about first.
   *
   *  Rows are EXISTING pairs. An agent the viewer has never messaged is absent
   *  (@shiki's ruling) — starting a line is the agent page's job. Refreshed by the
   *  viewer-scoped `direct_pairs_changed` signal, which is what makes a FIRST DM
   *  from a stranger appear without a reload. Read-only. */
  directPairs: '/api/direct-pairs',
  /** The single Attention eliminate primitive:
   *  every read-cleared Activity item collapses into one `observe(source_ref[])`
   *  use case that delegates the write back to the source aggregate (per-stream
   *  cursor / `notifications.read_at`). The legacy channel mark-read route was
   *  retired into this primitive (#task:1232). Owned by the Attention bounded
   *  context; human viewer-scoped. */
  attentionObserve: '/api/attention/observe',
  /** The viewer's cross-workspace BADGE roll-up (#task:1870) — the tiered
   *  presentation projection, NOT the raw unread 口径: `all`-tier conversations
   *  contribute their positional unread, `mentions`/`mute`-tier ones contribute
   *  only their directed (@me) unread. Deliberately a SEPARATE face from
   *  `messageUnreadSummary` so no existing "how many unread" answer changes.
   *  Refreshed by the viewer-scoped `attention_badge_updated` signal (absolute
   *  per-workspace patch) and, for its `direct_pairs` term, re-read on
   *  `direct_pairs_changed`. Read-only, human viewer-scoped, global. */
  attentionBadges: '/api/attention/badges',
  /** Every channel tier in ONE workspace in a single read (#task:1870):
   *  `?workspace_id=…` is REQUIRED — one batch instead of N per-channel GETs for
   *  the sidebar. Carries each channel's own `mode`, the workspace-suppressed
   *  `effective_mode`, and its directed unread count (what makes a quiet/silent
   *  row explainable, and since #task:1939 what surfaces it out of a collapsed
   *  workspace — the workspace aggregate is no longer displayed). Read-only,
   *  human viewer-scoped. */
  attentionModes: '/api/attention/modes',
  conversationLogDebug: (id: string) => `/api/conversations/${id}/log-debug`,
  agentActivity: (agentId: string) => `/api/agents/${encodeURIComponent(agentId)}/activity`,
  conversationAgentActivity: (id: string, agentId: string) =>
    `/api/conversations/${id}/agents/${agentId}/activity`,

  /** Members (agents + humans) of any conversation by id (channel /
   *  dm_channel / thread). Backs the chat-header member chips. The
   *  shape is the same `MemberDto[]` rendered into the agent system
   *  prompt, so renderer + prompt see the same source of truth. Self
   *  is NOT filtered server-side: the renderer drops the current
   *  human viewer (UI-self) — agent self has no UI meaning here.
   *  Permission: `canViewConversation` (workspace member or
   *  conversation member). */
  conversationMembers: (id: string) => `/api/conversations/${id}/members`,
  /** Remove one agent from a channel/thread conversation. This only
   *  mutates the conversation membership edge; workspace membership and
   *  the agent row stay intact. */
  conversationAgentMember: (id: string, agentId: string) =>
    `/api/conversations/${id}/members/agents/${agentId}`,
  /** Remove ANOTHER human from a channel (#task:1688). The third-party
   *  counterpart of `channelLeave`: same eligibility guards evaluated for the
   *  TARGET, plus workspace governance (`admin`) on the caller. Removing
   *  YOURSELF is not this route — `channelLeave` is, and it deliberately emits
   *  no system message. Workspace membership and the user row stay intact;
   *  only the conversation edge is removed. */
  conversationHumanMember: (id: string, userId: string) =>
    `/api/conversations/${id}/members/users/${userId}`,

  // attachments — upload via channel context, download by id.
  // Each upload creates an independent blob (no de-dup).
  channelAttachments: (id: string) => `/api/channels/${id}/attachments`,
  /** Distinct canonical-source senders for the channel Attachments panel's
   *  sender filter. Identity-only facets; same channel view permission as the
   *  list. A distinct path from the list route, and `attachment(:id)` lives
   *  under `/api/attachments`, so there is no route-ordering hazard. */
  channelAttachmentSenders: (id: string) => `/api/channels/${id}/attachments/senders`,
  attachment: (id: string) => `/api/attachments/${id}`,

  /** Global message unread summary across all workspace/sidebar items.
   *  GET -> `{ by_workspace: Record<workspaceId, number> }` */
  messageUnreadSummary: '/api/message-unread-summary',

  // saved messages
  saved: '/api/saved',
  search: '/api/search',
  messageSave: (id: string) => `/api/messages/${id}/save`,

  /** Message reactions (docs/reactions-design.md §5.1). POST adds, DELETE
   *  removes (both `{emoji}` in body); GET returns the lazy reactor detail
   *  (`{emoji, reactors:[{kind,id,display_name}]}[]`) for hover, kept off the
   *  message-window DTO per §6 perf. */
  messageReactions: (id: string) => `/api/messages/${id}/reactions`,

  // single message lookup — used by thread anchor preview to backfill
  // old rows whose metadata snapshot is missing (pre-PR4-fix data).
  message: (id: string) => `/api/messages/${id}`,

  // current viewer (the local human user). Pre-authentication the
  // endpoint returns the discriminated `RegistrationRequired` shape so
  // the renderer can mount the auth gate.
  users: '/api/users',
  relatedUsers: '/api/users/related',
  mePassword: '/api/me/password',
  me: '/api/me',
  /** GET: whether account deletion is offered and what still blocks it. */
  meDeletion: '/api/me/deletion',
  embeddedProfileSetup: '/api/me/embedded-profile-setup',
  meAvatar: '/api/me/avatar',
  /** GET `AvatarDesignResponse` for the caller's own avatar. */
  meAvatarDesign: '/api/me/avatar/design',
  /** Public avatar fetch for any user. Returns the blob bytes (auth
   *  via the standard principal header). The DTO's `avatar_url`
   *  points here; renderer resolves it via the same auth-headers
   *  + blob URL pattern used by attachments. Returns 404 when the
   *  user has no avatar set. */
  userAvatar: (id: string) => `/api/users/${id}/avatar`,

  // notifications (v1: cross-channel mention CC events)
  notifications: '/api/notifications',
  notificationsUnread: '/api/notifications/unread',
  notificationsRead: '/api/notifications/read',

  // tasks — human-facing task collection. Per docs/tasks-design.md. Path key =
  // task id (`tasks.id`); a task is a first-class aggregate, not a message
  // (v60).
  // POST-ONLY (#task:1891): human panel direct-create (`HumanCreateTaskInput`,
  // sourceless, membership + open fail-closed), delegating to the one
  // actor-agnostic `taskService` — the human create shares the agent CLI's
  // create semantics. There is NO human GET here: every human read is a bounded
  // face — the three board faces below for the board, `taskByNumber` for an
  // inline `#task:<n>` sigil, `/api/tasks/:id` for one task. The agent list
  // lives on its own internal route and is unaffected.
  tasks: '/api/tasks',
  // Human task BOARD read faces (#task:1884). Three separate GETs because the
  // board needs three DISTINCT facts, and each fact gets exactly one carrier:
  //   taskBoardColumn — one status column's keyset WINDOW (items + next_cursor).
  //   taskBoardCounts — the authoritative per-status TOTALS (one GROUP BY).
  //   taskBoardFacets — the filter menus' person option sets.
  // No face returns another's fact (a window carries no total; counts carry no
  // items), so a paginated board can never hold two disagreeing copies of one
  // number. All three interpolate the SAME population predicate
  // (repos/task-board-population.ts) — including the viewer-visibility gate,
  // and always INSIDE the query, before that face's LIMIT or aggregate: no face
  // post-filters rows out of a page it already cut, so narrowing/paging can
  // never widen what a viewer sees nor shorten a window that has more rows
  // behind it. These are HUMAN-only
  // and deliberately NOT reachable through `ListTasksQuerySchema`: that schema
  // is the AGENT list contract, and board pagination must not enter it.
  taskBoardColumn: '/api/tasks/board/column',
  taskBoardCounts: '/api/tasks/board/counts',
  taskBoardFacets: '/api/tasks/board/facets',
  // Human POINT LOOKUP by the board-scoped BUSINESS key
  // `(board_key, task_number)` — the identity an inline `#task:<n>` sigil
  // carries, which no other human face can express: the board faces are
  // status-partitioned and carry no `task_number` axis, and `GET /api/tasks/:id`
  // needs an id the sigil doesn't have. The board is named INDIRECTLY, by any
  // conversation on it (`?conversation_id=`): numbers are allocated per board
  // (`uniq_tasks_board_number`), and a DirectPair board has no workspace at all,
  // so a `workspace_id` axis could never name it (#task:2037 B5). The
  // `conversation_id` is a lookup key, never an authorization claim — the
  // viewer gate is the same one every other human face uses. Symmetric with the
  // agent CLI's by-number detail read, so a business-key lookup is a face on
  // both sides rather than a filtered list-of-one (#task:1891).
  taskByNumber: '/api/tasks/by-number',
  taskClaim: (taskId: string) => `/api/tasks/${taskId}/claim`,
  taskUnclaim: (taskId: string) => `/api/tasks/${taskId}/unclaim`,
  taskStatus: (taskId: string) => `/api/tasks/${taskId}/status`,
  /** Human edits a task's title/content (optimistic-concurrency CAS). */
  taskEdit: (taskId: string) => `/api/tasks/${taskId}/edit`,
  /** Human reads one task's full detail (content + provenance + nav). */
  taskDetail: (taskId: string) => `/api/tasks/${taskId}`,
} as const;

export const probePaths = {
  readyz: '/readyz',
} as const;

export const authPaths = {
  embeddedBootstrap: '/auth/embedded-bootstrap',
  signup: '/auth/signup',
  login: '/auth/login',
  logout: '/auth/logout',
  passwordRecoveryRequest: '/auth/password-recovery/request',
  passwordRecoveryReset: '/auth/password-recovery/reset',
} as const;

/** Canonical agent-global internal route family. Operation coordinates live in
 *  the request body/query/object id; the URL carries only the executing agent. */
export const internalPaths = {
  /** Agent posts a chat message; backend writes + distributes. */
  agentSendConversationMessage: (aid: string) => `/internal/agents/${aid}/send-message`,
  /** Agent reports that a stale send was intentionally abandoned. */
  agentResolveStaleMessage: (aid: string) => `/internal/agents/${aid}/stale-message-resolution`,
  /** Agent creates a new top-level channel in the same workspace. */
  agentCreateChannel: (aid: string) => `/internal/agents/${aid}/create-channel`,
  /** Agent reads message history at an explicit absolute target. */
  agentReadHistory: (aid: string) => `/internal/agents/${aid}/read-history`,
  /** Agent voluntarily follows a reply cluster (thread = root message id),
   *  `source='manual'`. Idempotent; a new follow backfills thread history. */
  agentFollowThread: (aid: string) => `/internal/agents/${aid}/follow-thread`,
  /** Agent stops following a reply cluster; re-followed automatically on a
   *  later mention/reply (§5). */
  agentUnfollowThread: (aid: string) => `/internal/agents/${aid}/unfollow-thread`,
  /** Agent lists workspaces it currently has member access to (regular
   *  workspaces via `workspace_members` ∪ its own `agent_home`). */
  agentListWorkspaces: (aid: string) => `/internal/agents/${aid}/list-workspaces`,
  /** Agent lists channels of a workspace it is currently a member of.
   *  Defaults to the source conversation's workspace when no
   *  `workspace_id` query param is supplied. */
  agentListChannels: (aid: string) => `/internal/agents/${aid}/list-channels`,
  /** Agent searches messages visible from current/workspace/channel scope. */
  agentSearchMessages: (aid: string) => `/internal/agents/${aid}/search-messages`,
  /** Agent adds an existing workspace member to its current channel. */
  agentAddConversationMember: (aid: string) => `/internal/agents/${aid}/members`,
  /** Agent reacts to a message with an emoji (POST add / DELETE remove,
   *  `{message_id, emoji}` body). Requires the `send_messages` capability —
   *  reaction is a write (docs/reactions-design.md §1). */
  agentReactMessage: (aid: string) => `/internal/agents/${aid}/react-message`,

  // Canonical task endpoints used by the Silverhand CLI.
  agentListConversationTasks: (aid: string) => `/internal/agents/${aid}/tasks`,
  agentCreateConversationTasks: (aid: string) => `/internal/agents/${aid}/tasks/create`,
  agentClaimConversationTasks: (aid: string) => `/internal/agents/${aid}/tasks/claim`,
  agentUnclaimConversationTask: (aid: string) => `/internal/agents/${aid}/tasks/unclaim`,
  agentUpdateConversationTaskStatus: (aid: string) => `/internal/agents/${aid}/tasks/update-status`,
  /** Agent edits a task's title/content (optimistic-concurrency CAS). */
  agentEditConversationTask: (aid: string) => `/internal/agents/${aid}/tasks/edit`,
  /** Agent reads one task's full detail (content + provenance + source_ref). */
  agentGetConversationTaskDetail: (aid: string) => `/internal/agents/${aid}/tasks/detail`,

  // Agent-owned schedules (cron-like reminders). Per-agent surface; the wake
  // destination is an explicit business value on create, not HTTP routing.
  agentScheduleCreate: (aid: string) => `/internal/agents/${aid}/schedules`,
  agentScheduleList: (aid: string) => `/internal/agents/${aid}/schedules/list`,
  agentScheduleCancel: (aid: string) => `/internal/agents/${aid}/schedules/cancel`,

  /** Agent uploads a local file to an explicit conversation target. */
  agentUploadAttachment: (aid: string) => `/internal/agents/${aid}/attachments`,
  /** Agent replaces the bytes of an attachment it created, in place. Same id,
   *  same stable local path; filename/content_type stay locked. Creator-only,
   *  same-conversation — the natural home for iterating a mockup without churning
   *  the reviewer's local address every round. */
  agentUpdateAttachment: (aid: string, attachmentId: string) =>
    `/internal/agents/${aid}/attachments/${attachmentId}`,
  /** Agent asks the backend where/how to read an attachment. Embedded can
   *  return a zero-copy local path; remote-safe callers may use the
   *  paired download route and cache bytes locally. */
  agentLocateAttachment: (aid: string, attachmentId: string) =>
    `/internal/agents/${aid}/attachments/${attachmentId}/locate`,
  /** Agent downloads attachment bytes after locate has proved same-channel access. */
  agentDownloadAttachment: (aid: string, attachmentId: string) =>
    `/internal/agents/${aid}/attachments/${attachmentId}/download`,

  /** Agent uploads its own avatar (multipart). Token middleware enforces
   *  `principal.agentId === aid`, so the route is structurally self-only.
   *  PR-2 mirrors the public `POST /api/agents/:id/avatar` write path
   *  for the agent-self branch of `canEditAgent`. */
  agentUploadAvatar: (aid: string) => `/internal/agents/${aid}/avatar`,
  /** Agent clears its own avatar. Idempotent; returns the updated DTO.
   *  Token middleware enforces self-only as above. */
  agentDeleteAvatar: (aid: string) => `/internal/agents/${aid}/avatar`,

  /** Observability-only: CLI fire-and-forget upload of a pre-flight
   *  failure (parse / strict-flag / unknown-noun) that never reached
   *  any other internal route. Logged as `cli.preflight_failure` and
   *  excluded from the `cli.invoke` success-rate stats. Best-effort:
   *  CLI does not retry and does not surface failures from this call. */
  agentCliTrace: (aid: string) => `/internal/agents/${aid}/cli-trace`,

  /** Agent reads its own profile MCP config (#task:1135). Returns
   *  `{ raw: string | null }` — null when the agent has no `mcp.json`.
   *  Token middleware enforces `principal.agentId === aid` (self-only).
   *  Backs `silverhand profile mcp get` and the agent-pane "Profile MCP"
   *  view (the pane calls the public mirror on `/api/agents/:id`). */
  agentProfileMcpGet: (aid: string) => `/internal/agents/${aid}/profile/mcp`,
  /** Agent replaces its own profile MCP config (#task:1135). Body is the
   *  raw JSON text; the backend validates it (fail-loud `invalid_config`)
   *  before dispatching the atomic host write. Self-only as above.
   *  Backs `silverhand profile mcp set`. */
  agentProfileMcpSet: (aid: string) => `/internal/agents/${aid}/profile/mcp`,
  /** Agent live-probes one of its own profile MCP servers (#task:1137). Body
   *  is `{ server_name, include_tools? }`; returns a `McpProbeResult`. The
   *  daemon opens a one-shot connection and closes it. Self-only via the
   *  internal principal middleware. Backs `silverhand profile mcp probe`. */
  agentProfileMcpProbe: (aid: string) => `/internal/agents/${aid}/profile/mcp/probe`,

  /** Agent reads its own profile metadata (#task:1178). Returns an
   *  `AgentProfileDto` (display_name / description / avatar_url + identity).
   *  Token middleware enforces `principal.agentId === aid` (self-only).
   *  Backs `silverhand profile get`. The avatar bytes themselves are
   *  served by the existing `agentAvatar` route via `avatar_url`. */
  agentProfileGet: (aid: string) => `/internal/agents/${aid}/profile`,
  /** Agent updates its own profile (#task:1178) in ONE multipart request —
   *  `display_name` / `description` text fields plus an optional avatar
   *  action (`avatar_action=set` with a `file` part, or `avatar_action=
   *  clear`). One handler applies the whole edit atomically (no partial
   *  write) and the avatar bytes ride a `file` part so they never base64
   *  into JSON; the avatar service stays the MIME/size/storage validator.
   *  Same URL as the GET above (method-distinguished). Self-only as above.
   *  Returns the updated `AgentProfileDto`. Backs `silverhand profile
   *  update`. */
  agentProfileUpdate: (aid: string) => `/internal/agents/${aid}/profile`,

  /** Agent reads ANOTHER agent's profile metadata (#task:1823), read-only.
   *  `aid` is still the EXECUTING agent (token-pinned); the target rides the
   *  `handle` query param, so the self routes above keep their structural
   *  self-only property untouched — read horizon and write authority are
   *  separate axes, not one URL with a wider gate.
   *
   *  Returns the SAME `AgentProfileDto` as the self read (one projection, so
   *  the two views cannot drift). Visibility is co-membership of a workspace
   *  the caller can reach: `workspace_id` defaults to the source
   *  conversation's workspace and a cross-workspace target is gated by
   *  `canAgentAccessWorkspace`, then `read_messages`. Every caller-side gate
   *  runs before the TARGET is resolved (query syntax is validated first — a
   *  400 talks about the request, not about who exists). Unknown handle, a
   *  handle that belongs to a human, and a target outside that workspace all
   *  answer an identical 404 — this endpoint must not become an existence
   *  oracle. Backs `silverhand profile get --handle <name>`. */
  agentGetPeerProfile: (aid: string) => `/internal/agents/${aid}/peer-profile`,
} as const;
