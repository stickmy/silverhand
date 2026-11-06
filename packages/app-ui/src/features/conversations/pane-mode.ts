/**
 * The chat pane's MODE, projected once from the conversation's placement
 * (#task:2039).
 *
 * shiki: «channel view 我觉得现在出现了不同的模式：分为 DM 和 channel 那种，相关的
 * 能力应该按照这个来收敛». Two long-term shapes fill the same pane —
 *
 *  - `workspace_channel` — a workspace container: roster, member management,
 *    channel identity (`#`), workspace-scoped refs and candidate searches;
 *  - `direct_message` — a `direct_pair`: two endpoints, no roster and no
 *    container, the counterpart IS the identity. Presence IS placement-keyed
 *    (#task:2123): a roster placement reads a member's state from the session
 *    serving that conversation, a DirectPair from the agent's global
 *    Availability (#task:2043). Which one applies follows from
 *    {@link ConversationPaneCapabilities.hasRoster} — see `presenceAuthorityFor`.
 *
 * — and the four dogfood findings behind this ticket were four different
 * components each guessing which one they were in (`workspaceId === null`, the
 * route, a local `isDirectPair` boolean). The projection happens HERE, once, and
 * every affordance reads {@link paneCapabilities} instead of re-deriving it.
 *
 * IT DECIDES UI CAPABILITY ONLY (@Cindy, #task:2039). Whether a read or write is
 * allowed stays the backend's answer; this module says which affordances a
 * placement can even show, so it must never be promoted into a front-end
 * permission truth.
 *
 * TOTAL OVER THE PLACEMENT UNION, with a `default` arm that is NOT dead. The
 * compiled alphabet is `workspace | direct_pair`, but this renderer reads it off
 * an HTTP DTO with no runtime validation and can be OLDER than the server it
 * talks to — a newer server may send a placement this build's alphabet does not
 * contain. Such a value projects to `unsupported`, which withholds every
 * placement-specific affordance, rather than inheriting channel chrome by
 * falling through. The arm is the point: an unknown placement fails safe.
 */
import type {
  AgentDto,
  ConversationPlacementKind,
  DirectPairCounterpart,
} from "@silverhand/shared";
import type { ConversationPaneDescriptor } from "./pane-descriptor";

/**
 * The mentionable identity of an agent — what the composer's `@` picker, avatar
 * prefetch, and send-time resolver read. A structural subset of both {@link AgentDto}
 * and the active-agent arm of {@link DirectPairCounterpart}, so a DM counterpart and
 * an owned agent fill one candidate list without a cast (#task:2147).
 */
export type MentionAgent = Pick<
  AgentDto,
  "id" | "name" | "display_name" | "avatar_url" | "runtime"
>;

export type ConversationPaneMode =
  /** A conversation inside a workspace container. `workspaceId` stays nullable
   *  because the descriptor's is: the pane paints what it was handed rather than
   *  asserting a coordinate, and the workspace-scoped affordances are gated on
   *  {@link ConversationPaneCapabilities.workspaceScope} being non-null. */
  | { kind: "workspace_channel"; workspaceId: string | null }
  /** A `direct_pair`. `counterpart` is absent on carriers that don't resolve one
   *  (a deep link that only knows the conversation id), which the header answers
   *  with the title alone rather than with channel chrome. */
  | { kind: "direct_message"; counterpart: DirectPairCounterpart | null }
  /** A placement this pane has no mode for. Carries the placement so a future
   *  arm is named by whoever adds it, not silently absorbed. */
  | { kind: "unsupported"; placement: ConversationPlacementKind };

/** Project the pane's conversation onto its mode. The ONE place placement is
 *  read for UI shape. */
export function projectConversationPaneMode(
  conversation: ConversationPaneDescriptor,
): ConversationPaneMode {
  switch (conversation.placement_kind) {
    case "workspace":
      return { kind: "workspace_channel", workspaceId: conversation.workspace_id };
    case "direct_pair":
      return {
        kind: "direct_message",
        counterpart: conversation.counterpart ?? null,
      };
    default:
      return { kind: "unsupported", placement: conversation.placement_kind };
  }
}

/** The placement a mode stands for — the inverse of the projection, so the two
 *  directions can be checked against each other and a mode can look up any
 *  placement-keyed rule without a second copy of the mapping. */
export function placementOfPaneMode(mode: ConversationPaneMode): ConversationPlacementKind {
  switch (mode.kind) {
    case "workspace_channel":
      return "workspace";
    case "direct_message":
      return "direct_pair";
    case "unsupported":
      return mode.placement;
  }
}

/**
 * What the pane may show, and what it may READ. Every field is a UI capability,
 * never a permission.
 *
 * Presence is deliberately NOT its own field here even though the AUTHORITY that
 * answers it IS placement-keyed (#task:2123): a roster placement reads the run
 * state of the session serving this conversation, a pair reads the agent's global
 * Availability (`agent_availability_changed`, #task:2043 → `projectGlobalPresence`).
 * That difference is not a second capability, because it follows exactly from
 * {@link hasRoster} — both the membership answer (`Not here`) and the runtime read
 * are roster powers, and `presenceAuthorityFor` derives the choice from that one
 * field. A `presence` enum beside it would be a second way to say the same thing,
 * free to disagree with the reads {@link rosterQueryTarget} actually starts.
 */
export interface ConversationPaneCapabilities {
  /** The identity primitive the header wears: the channel `#`, the counterpart's
   *  face, or the title alone when the placement has neither. */
  headerIdentity: "channel_hash" | "counterpart" | "title_only";
  /** Whether this placement HAS a roster — the member strip, the members button
   *  and its dialog, the add-member picker, the per-conversation agent
   *  management controls, AND the `conversation_agents` / runtime reads that feed
   *  them. A pair's two endpoints are not a roster, so it has none of it (shiki,
   *  #task:2039).
   *
   *  It also decides WHICH AUTHORITY the presence line reads: a roster-bearing
   *  placement reads the roster's runtime slot (and can say `Not here` for an
   *  agent not on it), a pair reads global Availability and never says either
   *  (#task:2123).
   *
   *  It gates the READS, not just the painting (@Cindy, #task:2039 review): a
   *  surface that starts the query and then renders nothing because the response
   *  happens to be empty has moved its capability into the data, where the next
   *  placement with a non-empty roster silently inherits it. See
   *  {@link rosterQueryTarget}. */
  hasRoster: boolean;
  /** The workspace that workspace-scoped affordances resolve in — `#channel`
   *  refs, `#task:<n>` refs, in-workspace jumps — or `null` when there is none. */
  workspaceScope: string | null;
}

/**
 * Every placement-specific affordance withheld: no roster (so no roster read and
 * no `Not here`), no workspace scope, and the title alone for identity.
 *
 * ONE literal with two callers, deliberately: an unknown placement and a parent
 * that has not resolved yet are different facts with the same answer, and the
 * point of that answer is that it cannot drift into "channel by default".
 */
const WITHHELD_PANE_CAPABILITIES: ConversationPaneCapabilities = {
  headerIdentity: "title_only",
  hasRoster: false,
  workspaceScope: null,
};

const DIRECT_MESSAGE_PANE_CAPABILITIES: ConversationPaneCapabilities = {
  headerIdentity: "counterpart",
  hasRoster: false,
  workspaceScope: null,
};

// One object per workspace scope. The capabilities are provided by context to
// every message row, so a fresh object per render would render them all again.
const workspaceChannelCapabilities = new Map<string | null, ConversationPaneCapabilities>();

export function paneCapabilities(mode: ConversationPaneMode): ConversationPaneCapabilities {
  switch (mode.kind) {
    case "workspace_channel": {
      let capabilities = workspaceChannelCapabilities.get(mode.workspaceId);
      if (!capabilities) {
        capabilities = {
          headerIdentity: "channel_hash",
          hasRoster: true,
          workspaceScope: mode.workspaceId,
        };
        workspaceChannelCapabilities.set(mode.workspaceId, capabilities);
      }
      return capabilities;
    }
    case "direct_message":
      return DIRECT_MESSAGE_PANE_CAPABILITIES;
    case "unsupported":
      return WITHHELD_PANE_CAPABILITIES;
  }
}

/**
 * The capabilities of a pane that is NOT a conversation of its own but hangs off
 * one — a thread (#task:2098). A thread has no placement: it shares its parent
 * conversation, so its capabilities ARE the parent's already-projected ones. This
 * function is the whole seam; a thread surface must never re-derive placement from
 * `workspaceId === null`, the route, or a local boolean, which is exactly how the
 * DM thread came to paint a channel's `Not here` for an online agent.
 *
 * A `null` parent is the pane rendering before its parent conversation has
 * resolved — a cold deep link to `?thread=<root>` whose thread stream has its own
 * query and can settle first. It withholds, rather than inheriting the
 * channel-shaped context default: during that window the surface shows no presence
 * line at all, which is the same thing it already does while a roster read is
 * unsettled, instead of asserting a membership fact it cannot know yet.
 */
export function inheritedPaneCapabilities(
  parent: ConversationPaneDescriptor | null,
): ConversationPaneCapabilities {
  if (!parent) return WITHHELD_PANE_CAPABILITIES;
  return paneCapabilities(projectConversationPaneMode(parent));
}

/**
 * What a surface with no pane around it may assume — the workspace-channel
 * capabilities, i.e. exactly what every one of them did before this ticket. It is
 * the context default, so adding the projection changed no existing behavior; a
 * placement-specific pane overrides it with its own projection.
 *
 * `workspaceScope` is null because a coordinate must be carried, never assumed.
 */
export const DEFAULT_PANE_CAPABILITIES: ConversationPaneCapabilities = paneCapabilities({
  kind: "workspace_channel",
  workspaceId: null,
});

/**
 * The conversation a roster read should target, or `null` for "do not read".
 *
 * Two independent facts disable a roster read and they compose here rather than
 * at each call site: the placement having no roster at all (capability), and the
 * viewer not being a member of one that exists (`gate.allowMemberEffects` — a
 * non-member browsing a public channel must not read its roster). Both hooks that
 * feed the roster surfaces take a nullable id and disable their query AND their
 * WS interest when handed `null`, so this is the off switch, not a filter applied
 * to a response that was already fetched.
 */
export function rosterQueryTarget(
  capabilities: ConversationPaneCapabilities,
  conversationId: string,
  allowMemberEffects: boolean,
): string | null {
  return capabilities.hasRoster && allowMemberEffects ? conversationId : null;
}

/**
 * Which TYPED directory the composer's `@` mention and `#` channel pickers — and
 * the send-time shorthand resolver they share — read their candidates from.
 * Projected EXHAUSTIVELY from {@link ConversationPaneMode}, so it can never be
 * re-derived from `workspaceId === null`, the route word "DM", or a lone
 * {@link ConversationPaneCapabilities.hasRoster} boolean.
 *
 * The boolean is the trap this type exists to close: `paneCapabilities` returns
 * `hasRoster:false` for a DirectPair AND for an unsupported/unresolved placement,
 * so `hasRoster ? workspaceMembers : viewerOwnedAgents` folds three real states
 * into two and hands the viewer's whole owned-agent directory to a cold-start or
 * unknown placement. Three arms keep them apart:
 *
 *  - `workspace` — a resolved roster-bearing placement. Candidates are that
 *    workspace's members / channels. Carries the workspace coordinate the two
 *    workspace-scoped queries need, so the leaf never re-derives it.
 *  - `viewer` — a resolved DirectPair. Candidates are the VIEWER's own directory
 *    (canonical-owned agents, the channels the viewer participates in), never a
 *    workspace roster.
 *  - `withheld` — placement unresolved or unsupported. NOT "resolved and empty":
 *    the authority is not yet KNOWN, so no directory query starts, no candidate is
 *    offered, and a hand-typed `@handle` stays PENDING (never settles to
 *    "addresses nobody") until a concrete arm resolves. Only the `workspace` and
 *    `viewer` arms can ever settle.
 */
export type ComposerDirectory =
  | {
      kind: "workspace";
      /** Always resolved: a `workspace_channel` mode whose `workspaceId` is still
       *  null projects to `withheld`, not here, so the workspace-scoped queries
       *  never run without their coordinate. */
      workspaceId: string;
      workspaceName: string | null;
      hasChannelNamespace: boolean;
    }
  | {
      kind: "viewer";
      /** The DM's counterpart when it is an active agent — a conversation fact, not
       *  gated on viewer ownership, so a counterpart the viewer does not own is still
       *  a candidate (filed under "In this channel"). null for a human / deleted /
       *  unresolved counterpart. The picker merges it with the viewer's owned agents
       *  by id and the send-time resolver reads that same set, so a picked non-owned
       *  counterpart still resolves on send. */
      counterpartAgent: MentionAgent | null;
    }
  | { kind: "withheld" };

/** Workspace-derived facts the `workspace` arm carries — resolved by the pane
 *  owner from the workspace list it already holds, never re-derived in the leaf
 *  (the same sibling-of-the-descriptor discipline the composer's props follow).
 *  Consulted ONLY in the `workspace` arm. */
export interface ComposerWorkspaceCoordinate {
  workspaceName: string | null;
  hasChannelNamespace: boolean;
}

/** Project a pane's mode onto its composer directory. Total over the mode union
 *  so a new placement is named by whoever adds it, not silently absorbed. */
export function composerDirectory(
  mode: ConversationPaneMode,
  workspace: ComposerWorkspaceCoordinate,
): ComposerDirectory {
  switch (mode.kind) {
    case "workspace_channel":
      // A workspace_channel whose workspace has not resolved yet carries no
      // coordinate; it is unresolved, not resolved-empty, so it withholds
      // rather than settling the workspace arm without an id.
      if (mode.workspaceId === null) return { kind: "withheld" };
      return {
        kind: "workspace",
        workspaceId: mode.workspaceId,
        workspaceName: workspace.workspaceName,
        hasChannelNamespace: workspace.hasChannelNamespace,
      };
    case "direct_message": {
      // The counterpart is a candidate only when it is an active agent: the deleted
      // arm carries no name/runtime to mention and a human has no `@`-agent identity,
      // so both project to null.
      const c = mode.counterpart;
      return {
        kind: "viewer",
        counterpartAgent:
          c && c.status === "active" && c.kind === "agent"
            ? {
                id: c.id,
                name: c.name,
                display_name: c.display_name,
                avatar_url: c.avatar_url,
                runtime: c.runtime,
              }
            : null,
      };
    }
    case "unsupported":
      return { kind: "withheld" };
  }
}

/** The composer directory of a pane that hangs off a parent conversation — a
 *  thread (#task:2098). A thread has no placement of its own, so it INHERITS the
 *  parent's projection; a `null` parent (a cold deep link before the parent
 *  resolves) is `withheld`, the same fail-safe {@link inheritedPaneCapabilities}
 *  uses, so the thread composer never re-derives the arm from `workspaceId` or the
 *  route. */
export function inheritedComposerDirectory(
  parent: ConversationPaneDescriptor | null,
  workspace: ComposerWorkspaceCoordinate,
): ComposerDirectory {
  if (!parent) return { kind: "withheld" };
  return composerDirectory(projectConversationPaneMode(parent), workspace);
}
