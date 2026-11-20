import { useMemo } from "react";
import type { AgentDto, UserDto } from "@silverhand/shared";
import { useAgents } from "@/features/agents/hooks/hooks";
import { useUsers, makeUserNameResolver } from "@/features/users/hooks";
import { useUserAvatarObjectUrls } from "@/features/users/avatar-hooks";
import {
  useWorkspaces,
  useChannelsForWorkspace,
  useArchivedChannelsForWorkspace,
  type ChannelListItem,
} from "@/features/workspaces/hooks";
import { useConversationMembers } from "@/features/conversations/use-conversation-members";
import {
  isBackendUnreachable,
  isRegistrationRequired,
  isUpdateRequired,
  useViewerAvatarObjectUrl,
  useViewerUser,
} from "@/features/session/hooks";
import type { UserNameResolver } from "@/features/session/current-user";

import type { ChannelReference } from "../components/channel-ref";

/**
 * Single source of truth for the lookup maps every message-rendering
 * surface needs (channel pane + thread reply-cluster pane). Both
 * `ChannelView`'s `MessageList` and `ThreadPanel` consume this so an
 * inline `@mention` / `#<channel>` / `#conv:` / `#task:` sigil resolves
 * IDENTICALLY no matter which pane renders the message.
 *
 * Previously `MessageList` and `ThreadPanel` each hand-built a slice of
 * this context; ThreadPanel's slice was incomplete (empty
 * `channelsByName`, no `#conv:` refs) which made thread messages render
 * sigils as inert text — the divergence this hook removes (#task:1052).
 *
 * Scope boundary: this hook owns the *identity + sigil-resolution* maps
 * and the per-user avatar batch (`humanAvatarUrls`). It deliberately
 * does NOT build the AGENT avatar batch (`agentAvatarUrls`): that one is
 * lifted by the caller so message authors, the member strip, and the
 * activity bubble share a single batch fetch (Cindy's batching
 * discipline — leaf surfaces must not double-fetch avatar blobs). The
 * caller passes its lifted `agentAvatarUrls` straight to `MessageItem`.
 *
 * The raw `liveChannels` / `archivedChannels` lists are exposed too so
 * non-render consumers (e.g. the export-screenshot channel title) reuse
 * the same already-fetched data instead of refetching.
 */
export interface MessageRenderContext {
  /** Author lookup by `from_agent_id` (avatar + display name). */
  agentsById: Map<string, AgentDto>;
  /** `@handle` mention lookup (agents). */
  agentsByName: Map<string, AgentDto>;
  /** User lookup by `users.id` for user-authored rows. */
  humansById: Map<string, UserDto>;
  /** `@handle` mention lookup (humans, same namespace as agents). */
  humansByName: Map<string, UserDto>;
  /** `#<channel_name>` ref lookup keyed by channel title. `null` value
   *  = ambiguity sentinel (multiple channels share that title). */
  channelsByName: Map<string, ChannelReference | null>;
  /** Per-user avatar object-URL batch (single fetch for the surface). */
  humanAvatarUrls: Map<string, string | null>;
  /** Current viewer's avatar object URL (or null). */
  viewerAvatarObjectUrl: string | null;
  /** Resolved viewer row, or undefined when registration isn't complete
   *  / backend is unreachable. Already guarded — callers pass it through
   *  to `MessageItem` without re-checking. */
  viewer: UserDto | undefined;
  /** `user_id` → display-name resolver for foreign-author fallbacks. */
  userResolver: UserNameResolver;
  /** Raw workspace channel lists (exposed for non-render consumers). */
  liveChannels: ChannelListItem[];
  archivedChannels: ChannelListItem[];
}

// Fallbacks for a query with no data (a disabled one, e.g. the channel lists of
// a conversation outside any workspace): a fresh `[]` each render would give
// every lookup map a new identity and re-render, and re-parse, every row.
const NO_AGENTS: AgentDto[] = [];
const NO_USERS: UserDto[] = [];
const NO_CHANNELS: ChannelListItem[] = [];

export function useMessageRenderContext(
  workspaceId: string | null,
  conversationId: string,
  membersEnabled = true,
): MessageRenderContext {
  const { data: agents = NO_AGENTS } = useAgents();
  const { data: users = NO_USERS } = useUsers();
  const { data: workspaces = [] } = useWorkspaces();
  // Resolve `#<channel_name>` against the message's workspace (not the
  // primary-selected one) so cross-workspace panes resolve their own
  // refs. Both queries short-circuit (`enabled: false`) until that workspace's
  // ROW has resolved, so they emit empty arrays in that window — which is also
  // the terminal state for a workspace-less conversation (#task:2035): a
  // `direct_pair` has no channel namespace, so `#name` refs there render as inert
  // text, exactly as an unresolvable one does.
  const workspaceResolved = workspaces.some((w) => w.id === workspaceId);
  const { data: liveChannels = NO_CHANNELS } = useChannelsForWorkspace(
    workspaceId,
    workspaceResolved,
  );
  const { data: archivedChannels = NO_CHANNELS } = useArchivedChannelsForWorkspace(
    workspaceId,
    workspaceResolved,
  );
  const { data: rawViewer } = useViewerUser();
  const viewer =
    rawViewer &&
    !isRegistrationRequired(rawViewer) &&
    !isBackendUnreachable(rawViewer) &&
    !isUpdateRequired(rawViewer)
      ? rawViewer
      : undefined;
  const viewerAvatarObjectUrl = useViewerAvatarObjectUrl();
  const humanAvatarUrls = useUserAvatarObjectUrls(users);
  // The conversation roster feeds ONLY @human-mention chip resolution
  // (`humansByName` below). It is a member surface: a non-member
  // browsing a public channel must not fetch it, so pass a null id to disable
  // both the query and its WS side-scope interest. Author display names come
  // from the global `useUsers()` (`humansById`), not the roster, so reading the
  // stream is unaffected — only unresolved @human chips degrade to plain text.
  //
  // A `direct_pair` DOES issue this read and it is not a mistake: the id is a
  // conversation, the placement ACL admits an endpoint, and the answer is a 200
  // with an EMPTY roster (a fixed-roster placement has no `conversation_users`
  // rows, "rosterless by construction"). So a pair resolves @human chips the
  // same way a non-member does — it doesn't — and nothing waits on it
  // (#task:2035).
  const conversationMembers = useConversationMembers(membersEnabled ? conversationId : null);

  const { agentsById, agentsByName } = useMemo(() => {
    const byId = new Map<string, AgentDto>();
    const byName = new Map<string, AgentDto>();
    for (const a of agents) {
      byId.set(a.id, a);
      byName.set(a.name, a);
    }
    return { agentsById: byId, agentsByName: byName };
  }, [agents]);

  const humansById = useMemo(() => {
    const byId = new Map<string, UserDto>();
    for (const user of users) byId.set(user.id, user);
    if (viewer) byId.set(viewer.id, viewer);
    return byId;
  }, [users, viewer]);

  const userResolver = useMemo(() => makeUserNameResolver(humansById), [humansById]);

  const humansByName = useMemo(() => {
    const byName = new Map<string, UserDto>();
    if (conversationMembers.status === "resolved") {
      for (const member of conversationMembers.members) {
        if (member.kind !== "human") continue;
        const user = humansById.get(member.user_id);
        if (user) byName.set(user.name, user);
      }
    }
    if (viewer) byName.set(viewer.name, viewer);
    return byName;
  }, [conversationMembers.status, conversationMembers.members, humansById, viewer]);

  // Channel-name lookup keyed by `title`. A shared title records the
  // ambiguity sentinel `null` so `<ChannelRef>` degrades to inert text
  // rather than guess a target.
  const channelsByName = useMemo(() => {
    const m = new Map<string, ChannelReference | null>();
    if (!workspaceResolved) return m;
    const consider = (c: ChannelListItem): void => {
      const title = c.title;
      if (m.has(title)) {
        m.set(title, null);
        return;
      }
      m.set(title, {
        id: c.id,
        title: c.title,
        closed_at: c.closed_at ?? null,
        archived_at: c.archived_at ?? null,
      });
    };
    for (const c of liveChannels) consider(c);
    for (const c of archivedChannels) consider(c);
    return m;
  }, [workspaceResolved, liveChannels, archivedChannels]);

  return {
    agentsById,
    agentsByName,
    humansById,
    humansByName,
    channelsByName,
    humanAvatarUrls,
    viewerAvatarObjectUrl,
    viewer,
    userResolver,
    liveChannels,
    archivedChannels,
  };
}
