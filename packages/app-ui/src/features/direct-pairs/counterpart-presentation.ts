/**
 * How a `direct_pair`'s counterpart renders (#task:2035). The pair's row and its
 * conversation header have no title of their own — a `direct_pair` holds no
 * `title_key`, and the list DTO deliberately carries none — so the counterpart IS
 * the name, and this is the one place that resolution lives.
 *
 * It maps onto the EXISTING author-presentation vocabulary
 * (`features/messages/model/author-presentation.ts`) rather than growing a second one: the
 * counterpart union has exactly the arms an author has (an active agent with a
 * runtime, an active human, and a principal that no longer exists), and
 * `AuthorAvatarView` already renders all four `AuthorAvatar` variants including the
 * neutral glyph + `DeletedActorMarker`. Reusing the union is what keeps a DM row's
 * face identical to the same actor's byline in the stream.
 *
 * The one place it CANNOT reuse the author rules is the deleted arm's LABEL. A
 * message author snapshots `author_name` at send time, so a deleted author stays
 * nameable and reads "<name> (deleted account)". An endpoint snapshots nothing (the
 * coordinate is the id and only the id), so there is no name to recover and the
 * label is kind-generic. This module owns that copy, because UI text is never
 * outsourced to the backend.
 *
 * Vocabulary bridge: the endpoint kind is `'human'` (a placement coordinate's own
 * spelling) while the avatar variant vocabulary is `'user'` (the message-author
 * one). The translation happens HERE, once, instead of at each rendering surface.
 */
import type { AgentDto, UserDto, DirectPairCounterpart } from "@silverhand/shared";
import type {
  AuthorAvatar,
  AuthorAvatarSources,
} from "@/features/messages/model/author-presentation";
import { t } from "@/lib/i18n";

export interface CounterpartPresentation {
  /** The name to render — the pair's whole title. */
  label: string;
  /** The ENDPOINT kind, kept in the endpoint's own spelling. */
  kind: "agent" | "human";
  status: "active" | "deleted";
  /** Only a deleted counterpart shows the quiet marker, exactly like a byline. */
  isDeleted: boolean;
  /** The actor id for click-to-profile, or null when there is no profile to open.
   *  A deleted principal is not navigable — the same rule bylines follow. */
  navigableId: string | null;
  avatar: AuthorAvatar;
}

/** Resolve a counterpart into its presentation. Pure. */
export function presentCounterpart(counterpart: DirectPairCounterpart): CounterpartPresentation {
  if (counterpart.status === "deleted") {
    return {
      // Nothing about an endpoint is snapshotted, so this is as specific as the
      // truth allows. "Deleted", not "Unknown": the server proved the principal
      // is gone (the endpoint column still names it), which is a stronger claim
      // than a byline's `unresolved`.
      label:
        counterpart.kind === "agent" ? t("directPairs.deletedAgent") : t("directPairs.deletedUser"),
      kind: counterpart.kind,
      status: "deleted",
      isDeleted: true,
      navigableId: null,
      avatar:
        counterpart.kind === "agent" ? { variant: "neutral-agent" } : { variant: "neutral-user" },
    };
  }
  // Same label rule as a byline (`presentAuthor`): the display name when the
  // person set one, the handle otherwise.
  const label = counterpart.display_name.trim() || counterpart.name;
  return {
    label,
    kind: counterpart.kind,
    status: "active",
    isDeleted: false,
    navigableId: counterpart.id,
    avatar:
      counterpart.kind === "agent"
        ? { variant: "agent", id: counterpart.id, runtime: counterpart.runtime }
        : { variant: "user", id: counterpart.id },
  };
}

/**
 * The counterpart of a line that does not exist yet (#task:2039): the agent the
 * route names, read as if the pair were already there.
 *
 * A pre-mint draft has no `DirectPairListItem` to carry a counterpart, and the two
 * surfaces that show one (the draft pane's header and the sidebar's transient
 * draft row) must not each invent their own — the whole point of the draft state is
 * that NOTHING moves when the first send converges it onto the real pair, and that
 * only holds if the face and the label come from this one rule.
 *
 * Always the `active` arm: the viewer is reading this agent's page, so the DTO
 * exists by construction. Nothing is persisted or pre-allocated by calling this —
 * it is a projection of an agent the renderer already fetched.
 */
export function counterpartFromAgent(
  agent: Pick<AgentDto, "id" | "name" | "display_name" | "avatar_url" | "runtime">,
): DirectPairCounterpart {
  return {
    status: "active",
    kind: "agent",
    id: agent.id,
    name: agent.name,
    display_name: agent.display_name,
    avatar_url: agent.avatar_url,
    runtime: agent.runtime,
  };
}

/**
 * The avatar-byte sources for a whole list of pairs, split by kind — the same
 * `{ id, avatar_url }` shape the agent/user object-url batch hooks accept, and the
 * same contract as `collectAuthorAvatarSources`: ONE batch per surface, resolved
 * over a stable source set, so a leaf row never fetches its own bytes.
 *
 * Only the `active` arm contributes: a deleted endpoint has no `avatar_url` to
 * fetch and renders a neutral glyph. Deduped by id, which also covers the (legal)
 * case of one principal appearing in more than one row.
 *
 * Takes anything that CARRIES a counterpart, not `DirectPairListItem` specifically:
 * the agent page's Chats rows are pair rows of a different DTO (the owner-audit
 * summary) and must resolve their faces through this same one batch rather than a
 * parallel copy of it.
 */
export function collectCounterpartAvatarSources(
  pairs: readonly { counterpart: DirectPairCounterpart }[],
): AuthorAvatarSources {
  const agents = new Map<string, string | null>();
  const users = new Map<string, string | null>();
  for (const pair of pairs) {
    const c = pair.counterpart;
    if (c.status !== "active") continue;
    if (c.kind === "agent") agents.set(c.id, c.avatar_url);
    else users.set(c.id, c.avatar_url);
  }
  return {
    agents: Array.from(agents, ([id, avatar_url]) => ({ id, avatar_url })),
    users: Array.from(users, ([id, avatar_url]) => ({ id, avatar_url })),
  };
}

export function counterpartFromHuman(user: UserDto): DirectPairCounterpart {
  return {
    status: "active",
    kind: "human",
    id: user.id,
    name: user.name,
    display_name: user.display_name,
    avatar_url: user.avatar_url,
    runtime: null,
  };
}
