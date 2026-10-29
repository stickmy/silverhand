/**
 * Pure selectors for the right-hand secondary pane.
 *
 * PR-1 introduced the URL → descriptor → visible/focused matrix.
 * PR-2 extended the `side` source to cross-workspace conversation ids by
 * routing through `GET /api/conversations/:id/ref`. #task:2038 widened that
 * again: the source is now `useConversationIdentity(id)`, which composes the
 * workspace face with the viewer's own direct-pair list, so a `?side=<private
 * line>` resolves instead of rendering "deleted or never existed".
 *
 * These functions are deliberately pure so that:
 *   1. app.tsx stays thin — just plumbing route state + the identity
 *      resolution into these helpers.
 *   2. Unit tests can exercise the URL → state matrix without
 *      mounting React or React Query.
 *
 * Vocabulary:
 *
 *   - `secondaryDescriptor` — what the right pane shows. The only source
 *     is `?side=<id>` (overlay; resolves any placement the viewer can see).
 *     The descriptor's status reflects the resolution lifecycle
 *     (loading/resolved/unauthorized/not_found/unresolved).
 *   - `visibleConversations` — set of conversations whose pane is
 *     currently on screen. Drives message read-cursor advancement.
 *     Fullscreen collapses to just the displayed pane; a non-resolved
 *     side contributes no read cursor — placeholder pane has no real
 *     conversation to mark read.
 *   - `focusedConversation` — the *one* conversation the user is
 *     considered to be focused on. Drives toast suppression and the IPC
 *     `setActiveConversation` payload (which only accepts a single id).
 *     Focus is a PASSIVE surface — it does NOT clear notification badges
 *     / write `read_at` (surfaced ≠ eliminated, #task:1244). Rule:
 *     resolved secondary > primary; non-resolved secondary doesn't take
 *     focus.
 */

import {
  conversationPaneDescriptorFromIdentity,
  type ConversationIdentityResolution,
} from "@/features/conversations/conversation-identity";
import type { ConversationPaneDescriptor } from "@/features/conversations/pane-descriptor";

export type SecondaryDescriptor =
  | {
      source: "side";
      status: "resolved";
      id: string;
      /** Everything the pane needs about the conversation, in the ONE shape
       *  `ChannelView` takes. Built by
       *  {@link conversationPaneDescriptorFromIdentity} so a private line in the
       *  side pane is the same descriptor the primary DM pane renders — this used
       *  to be a second, narrower record declared right here, which could not
       *  express a workspace-less placement at all. */
      descriptor: ConversationPaneDescriptor;
      /** Whether this conversation's container has a `#` channel namespace —
       *  false for a workspace-less placement, which is also what the pane reads
       *  as "not a top-level channel-like conversation" (no rename / archive /
       *  visibility). Derived from the identity's own discriminant, so the pane
       *  chain never carries a container kind (#task:2078). */
      hasChannelNamespace: boolean;
    }
  | { source: "side"; status: "loading"; id: string }
  | { source: "side"; status: "unauthorized"; id: string }
  | { source: "side"; status: "not_found"; id: string }
  | { source: "side"; status: "unresolved"; id: string };

export interface SecondaryRouteInputs {
  /** From `useRouteState().sideConversationId`. */
  sideConversationId: string | null;
}

/**
 * Decide what the right-hand pane should show given URL state and the
 * resolution of the `?side` id (when set).
 *
 * The only secondary source is `?side=<id>`. The selector never produces
 * a descriptor that requires "loading + null id" — non-resolved descriptors
 * always carry the id so the placeholder can render `#conv:<id>` and offer a
 * Close button.
 *
 * `?side` resolves through {@link useConversationIdentity}, which composes BOTH
 * identity faces: a workspace conversation (any workspace, not just the primary
 * one) and a `direct_pair` from the viewer's own complete list. The resolved
 * branch therefore carries a pane descriptor whose `workspace_id` may be null —
 * a private line has no workspace — so ChannelView switches its workspace-scoped
 * hooks (or withholds them entirely) without a second round-trip.
 *
 * `sideIdentity === null` is treated as "the resolution hook hasn't been read yet"
 * (i.e. the React component hasn't run yet); we surface this as `loading` so the
 * URL state and the descriptor never disagree across renders.
 */
export function selectSecondaryDescriptor(
  route: SecondaryRouteInputs,
  sideIdentity: ConversationIdentityResolution | null,
): SecondaryDescriptor | null {
  if (!route.sideConversationId) return null;
  const id = route.sideConversationId;
  const resolution = sideIdentity ?? { status: "loading" as const };
  if (resolution.status !== "resolved") {
    return { source: "side", status: resolution.status, id };
  }
  // Resolved — pull all routing metadata out of the identity so the descriptor
  // is the single source of truth for downstream wiring (no second hop into the
  // channels list / workspace store).
  return {
    source: "side",
    status: "resolved",
    id:
      resolution.identity.placement === "workspace"
        ? resolution.identity.ref.id
        : resolution.identity.pair.conversation_id,
    descriptor: conversationPaneDescriptorFromIdentity(resolution.identity),
    // The one bit the pane chain reads, taken straight off the identity's own
    // discriminant: a workspace placement has a `#` namespace, a private line has
    // none. The carrier's container kind is NOT consulted — nothing downstream
    // (ChannelView, ThreadPanel, Composer) ever distinguished the two kinds, so
    // reading it would only give a second spelling of the same bit (#task:2078;
    // the read-through of the published word it replaced landed in #task:2070).
    hasChannelNamespace: resolution.identity.placement === "workspace",
  };
}

/** ChannelView-shaped descriptor for a conversation. */
export type SecondaryConversationDescriptor = ConversationPaneDescriptor;

export interface SecondaryViewInputs {
  /** ChannelView `conversation` prop value. Null when the secondary
   *  pane should not render a real ChannelView (no secondary, or a
   *  non-resolved `?side`). */
  conversationDescriptor: ConversationPaneDescriptor | null;
  /** ChannelView `hasChannelNamespace` prop value. For a resolved `?side` this
   *  comes from the identity (false for a workspace-less placement); otherwise it
   *  falls through to the primary's bit. */
  hasChannelNamespace: boolean;
  /** Workspace id to bake into the secondary archive mutation
   *  invalidation. Cross-workspace secondary needs the foreign
   *  workspace id so the `['workspaces', wid, 'channels']` cache
   *  invalidation hits the right entry. Falls back to the primary
   *  workspace id for non-resolved cases, and is null for a placement that
   *  has no workspace to invalidate (a `direct_pair`, which no archive door
   *  accepts anyway — `hasChannelNamespace: false` withholds the affordance). */
  archiveWorkspaceId: string | null;
}

export interface SecondaryViewMappingInputs {
  secondary: SecondaryDescriptor | null;
  /** Whether the PRIMARY conversation's container has a channel namespace, used
   *  as the fallback for a non-resolved `?side` (a resolved `?side` carries its
   *  own bit so this is rarely consulted in practice). */
  primaryHasChannelNamespace: boolean;
  /** Primary workspace id, used as archive-invalidation target when
   *  there is no resolved secondary. */
  primaryWorkspaceId: string | null;
}

/**
 * Pure mapping from a SecondaryDescriptor to the props ChannelView
 * consumes for the secondary pane. Extracted so the cross-workspace
 * wiring is unit-testable end-to-end without a renderHook setup.
 *
 * Cindy review of PR-2: "测试里加一个跨 workspace side 的集成点：
 * `?side=<other workspace channel>` resolved 后，ChannelView 收到
 * other workspace id/kind/path，focused IPC 也用 other workspace
 * id。这个比只测 hook 更能防回归。" — that's this helper.
 */
export function selectSecondaryViewInputs(inputs: SecondaryViewMappingInputs): SecondaryViewInputs {
  const { secondary, primaryHasChannelNamespace, primaryWorkspaceId } = inputs;

  if (!secondary) {
    return {
      conversationDescriptor: null,
      hasChannelNamespace: primaryHasChannelNamespace,
      archiveWorkspaceId: primaryWorkspaceId,
    };
  }

  // `?side` resolved: the descriptor is already built from the identity, so a
  // cross-workspace channel needs no lookup hit and a private line needs no
  // workspace at all.
  if (secondary.source === "side" && secondary.status === "resolved") {
    return {
      conversationDescriptor: secondary.descriptor,
      hasChannelNamespace: secondary.hasChannelNamespace,
      archiveWorkspaceId: secondary.descriptor.workspace_id,
    };
  }

  // Non-resolved `?side` — placeholder pane is rendered separately;
  // no ChannelView descriptor.
  return {
    conversationDescriptor: null,
    hasChannelNamespace: primaryHasChannelNamespace,
    archiveWorkspaceId: primaryWorkspaceId,
  };
}

/** Conversation pointer used by the read-cursor / focus selectors. */
export interface ConversationPointer {
  id: string;
  rosterPolicy: "open" | "fixed";
}

export interface VisibleConversations {
  /** The base channel pane. Hidden only when a secondary takes over the
   *  full viewport (fullscreen=1 + resolved secondary). */
  primary: ConversationPointer | null;
  /** The secondary pane. Null when there is no secondary or when the
   *  secondary is non-resolved (loading / unauthorized / not_found —
   *  placeholder pane has no real conversation to mark read). */
  secondary: ConversationPointer | null;
}

export interface VisibleSelectionInputs {
  primary: ConversationPointer | null;
  secondary: SecondaryDescriptor | null;
  fullscreen: boolean;
}

/**
 * Compute which conversations are currently *visible* to the user.
 * Drives `markMessageChannelRead` — a conversation must be on screen
 * for its read cursor to advance.
 *
 * Rules:
 *   - No secondary             → only primary visible.
 *   - Resolved secondary       → both visible (split) unless fullscreen,
 *                                in which case only the secondary.
 *   - Non-resolved secondary   → only primary visible (placeholder pane
 *     (loading/unauthorized/      has no conversation to mark read).
 *      not_found)
 *   - Fullscreen + no secondary → only primary visible (fullscreen is a
 *                                no-op without a secondary).
 */
export function selectVisibleConversations(inputs: VisibleSelectionInputs): VisibleConversations {
  const { primary, secondary, fullscreen } = inputs;
  if (!secondary) {
    return { primary, secondary: null };
  }
  if (secondary.status !== "resolved") {
    // Placeholder occupies the slot visually but has no conversation
    // id to mark read. Primary is still visible behind it.
    return { primary, secondary: null };
  }
  const secondaryPointer: ConversationPointer = {
    id: secondary.id,
    rosterPolicy: secondary.descriptor.roster_policy,
  };
  if (fullscreen) {
    return { primary: null, secondary: secondaryPointer };
  }
  return { primary, secondary: secondaryPointer };
}

export interface FocusedConversation {
  id: string;
  rosterPolicy: "open" | "fixed";
  /** Workspace id for the focused conversation — the resolved secondary's own
   *  coordinate (cross-workspace capable, and `null` for a workspace-less
   *  placement) or the caller's `primaryWorkspaceId` for the primary case. */
  workspaceId: string | null;
}

export interface FocusSelectionInputs {
  primary: ConversationPointer | null;
  primaryWorkspaceId: string | null;
  secondary: SecondaryDescriptor | null;
  fullscreen: boolean;
}

/**
 * Compute the *single* "focused" conversation. Drives:
 *   - `useNotificationToasts` — no notification for it while the window has
 *     focus (user can already see it).
 *
 * Focus drives the above ONLY; it does NOT clear notification badges /
 * write `read_at` — focus is a passive surface (surfaced ≠ eliminated,
 * #task:1244). Directed claims clear only via active eliminate intent.
 *
 * Rule (preserves the legacy `openThreadId ?? selectedChannelId` semantic):
 *   - Resolved secondary (split or fullscreen) wins.
 *   - Non-resolved secondary doesn't take focus — primary keeps it.
 *   - No secondary → primary.
 *   - Nothing visible → null.
 */
export function selectFocusedConversation(
  inputs: FocusSelectionInputs,
): FocusedConversation | null {
  const { primary, primaryWorkspaceId, secondary, fullscreen } = inputs;
  if (secondary && secondary.status === "resolved") {
    return {
      id: secondary.id,
      rosterPolicy: secondary.descriptor.roster_policy,
      workspaceId: secondary.descriptor.workspace_id,
    };
  }
  // Fullscreen with no resolved secondary collapses to primary; same as
  // no-secondary case. (Fullscreen is meaningless without something to
  // fullscreen onto, so we treat it as a no-op rather than hiding
  // primary too.)
  void fullscreen;
  if (primary) {
    return {
      id: primary.id,
      rosterPolicy: primary.rosterPolicy,
      workspaceId: primaryWorkspaceId,
    };
  }
  return null;
}

/**
 * Which conversation the SIDEBAR paints as selected.
 *
 * A resolved secondary holds focus, so it owns the highlight — but only when its
 * row is actually in the sidebar the viewer is looking at, otherwise the primary's
 * row would go dark for a row that lights up nowhere. The two placements answer
 * "is my row on screen?" differently, and that is the whole content of this rule:
 *
 *   - a WORKSPACE conversation is listed under its own workspace, so it owns the
 *     highlight only when that workspace is the one expanded in the tree. A
 *     cross-workspace `?side` leaves the primary's row filled rather than
 *     ghost-filling a same-id row under a different parent.
 *   - a `direct_pair` is listed in DIRECT MESSAGES, which is workspace-independent
 *     and always rendered — so its row is on screen whatever the tree shows, and it
 *     takes the highlight. Comparing its null workspace to the selected workspace
 *     would be an accident either way: null-vs-null (viewer on a DM route) would
 *     light it up for the wrong reason, and null-vs-`w-1` would hide a row that is
 *     right there.
 */
export function selectSidebarSelectedConversationId(inputs: {
  /** What the sidebar would paint with no secondary — the primary selection. */
  primarySelectedChannelId: string | null;
  secondary: SecondaryDescriptor | null;
  primaryWorkspaceId: string | null;
}): string | null {
  const { primarySelectedChannelId, secondary, primaryWorkspaceId } = inputs;
  if (!secondary || secondary.status !== "resolved") {
    return primarySelectedChannelId;
  }
  const workspaceId = secondary.descriptor.workspace_id;
  if (workspaceId === null) return secondary.id;
  return workspaceId === primaryWorkspaceId ? secondary.id : primarySelectedChannelId;
}

/**
 * Should the right pane fully cover the viewport (hiding primary)?
 *
 * Only `?fullscreen=1` *with a resolved secondary* collapses to a
 * single-pane render. Non-resolved side + fullscreen falls back to the
 * split view (primary + placeholder) so what's on screen matches the
 * `visibleConversations` / `focusedConversation` selectors above —
 * those keep primary in the visible/focused set whenever secondary is
 * non-resolved (a placeholder has no conversation to mark read or
 * silence toasts for). Without this gate, fullscreen + non-resolved
 * would hide primary in the DOM while still advancing primary's read
 * cursor — Cindy review of commit `e9278a7` flagged this as a
 * state/render fork. (Focus no longer touches notification badges at
 * all — #task:1244 — so only the read-cursor fork remains.)
 *
 * Pure helper so the render-time decision can be unit-tested without
 * mounting React.
 */
export function shouldSecondaryFullscreen(
  secondary: SecondaryDescriptor | null,
  fullscreen: boolean,
): boolean {
  return fullscreen && secondary?.status === "resolved";
}
