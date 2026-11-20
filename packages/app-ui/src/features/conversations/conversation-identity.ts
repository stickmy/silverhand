/**
 * WHAT a conversation id names, across every placement — the renderer half of
 * #task:2038 (ruling A).
 *
 * There are two faces, and neither one alone can answer the question:
 *
 *  - `GET /api/conversations/:id/ref` is the WORKSPACE arm and only that. Its
 *    404 for a workspace-less placement is the correct answer, not a gap
 *    (`ConversationRefSchema.placement_kind` is `z.literal('workspace')`, so a
 *    private line is not even expressible in that DTO).
 *  - `GET /api/direct-pairs` is the only face that can name a `direct_pair`, and
 *    it is a COMPLETE SET for the viewer — no pagination, no cap, no truncation
 *    (that contract is written down beside the DTO and pinned by
 *    `tests/routes/direct-pairs.test.ts`). Completeness is what makes an
 *    ABSENCE decisive: the viewer is not an endpoint of it, so the reference
 *    may degrade to inert text.
 *
 * Composing them here — once — is what keeps the `#conv:<id>` chip and the
 * conversation pane from drifting: before this, both read the workspace face
 * alone, so every private line rendered as "deleted or never existed" while
 * sitting in the viewer's own DIRECT MESSAGES list.
 *
 * Precedence, and why:
 *
 *  1. a pair row IN HAND wins outright. It is the viewer's own row, and the
 *     workspace face necessarily 404s for the same id, so there is nothing to
 *     wait for and nothing to reconcile.
 *  2. the workspace face's own answers (200 / 403) are terminal. A 403 says the
 *     conversation IS in the workspace arm and refuses; a pair cannot 403.
 *  3. only a 404 consults the pair face, and it may claim `not_found` ONLY once
 *     that face has answered. This is the whole bug: a 404 arriving before the
 *     pair list has answered used to be reported as deletion.
 *  4. a workspace read that FAILED (5xx / transport, retry budget spent) settles
 *     into `unresolved` — never `loading` (a skeleton with nothing behind it) and
 *     never `not_found` (it proves nothing, so it may not borrow the
 *     completeness contract that licenses a claim about deletion).
 *
 * NOTE on retained rows. This module renders from rows it holds even while a
 * refetch is in flight — the opposite of {@link
 * import('@/features/app-shell/chat-place-restore').directPairAuthorityFromQuery}, which
 * collapses retained rows to `pending` on purpose. The two are asking different
 * questions: RESTORE routes the viewer somewhere and a navigation cannot be
 * taken back, so it may only act on a re-answered list; LABELLING draws a chip
 * that is already on screen, so discarding held rows would blink every DM
 * reference back to `#conv:<id>` on each invalidation of the list. Same
 * `ready > resolving` precedence as `projectDirectPairPane`.
 */
import type { ConversationRef, DirectPairListItem } from "@silverhand/shared";
import {
  useConversationById,
  type ConversationByIdResult,
} from "@/features/conversations/use-conversation-by-id";
import { useDirectPairsList, type DirectPairListQueryFacts } from "@/features/direct-pairs/hooks";
import { presentCounterpart } from "@/features/direct-pairs/counterpart-presentation";
import {
  directPairPaneDescriptor,
  type ConversationPaneDescriptor,
} from "@/features/conversations/pane-descriptor";

/** A resolved conversation id, discriminated by the face that resolved it. Two
 *  arms rather than one widened record: a `direct_pair` has no workspace
 *  coordinate and a workspace conversation has no counterpart, so a single flat
 *  shape would make both halves optional and let an impossible pair through
 *  (the narrowing #task:2038 B1 spent a commit removing from the wire DTO). */
export type ConversationIdentity =
  | { placement: "workspace"; ref: ConversationRef }
  | { placement: "direct_pair"; pair: DirectPairListItem };

/**
 * The FIVE things a conversation id can turn out to be, from the renderer's side.
 *
 *  - `resolved` — one of the two faces named it.
 *  - `loading` — a face that could still name it has not answered.
 *  - `unauthorized` — the workspace arm refuses (403). Terminal.
 *  - `not_found` — both faces answered SUCCESSFULLY and neither names it.
 *    Terminal, and the only status licensed to say "deleted or never existed":
 *    it rests on the workspace arm's 404 plus the direct-pair list's
 *    completeness contract.
 *  - `unresolved` — a read that would have answered FAILED, and nothing is in
 *    flight. Either face can put us here: the pair read failing under a
 *    workspace 404, or the workspace read settling in a 5xx / transport failure
 *    with its retry budget spent (@Cindy, #task:2038 B1). Nothing was proven, so
 *    this is neither `loading` (a skeleton that never ends) nor `not_found` (a
 *    false claim about deletion).
 */
export type ConversationIdentityStatus =
  | "loading"
  | "resolved"
  | "unauthorized"
  | "not_found"
  | "unresolved";

export type ConversationIdentityResolution =
  | { status: "resolved"; identity: ConversationIdentity }
  | { status: "loading" }
  | { status: "unauthorized" }
  | { status: "not_found" }
  | { status: "unresolved" };

/** Pure composition of the two faces. `id` is needed because the private-line
 *  face is a LIST: the caller may not pre-look-up the row, or the lookup rule
 *  would be re-typed per surface. */
export function resolveConversationIdentity(
  id: string,
  workspaceFace: ConversationByIdResult,
  pairFacts: DirectPairListQueryFacts,
): ConversationIdentityResolution {
  const pair = pairFacts.rows?.find((p) => p.conversation_id === id);
  if (pair) {
    return { status: "resolved", identity: { placement: "direct_pair", pair } };
  }
  if (workspaceFace.status === "resolved" && workspaceFace.ref) {
    return {
      status: "resolved",
      identity: { placement: "workspace", ref: workspaceFace.ref },
    };
  }
  if (workspaceFace.status === "unauthorized") return { status: "unauthorized" };
  if (workspaceFace.status === "loading") return { status: "loading" };
  // The workspace arm has SETTLED without naming it: either a 404 (the correct
  // answer for a private line) or a failed read (`unresolved`). Either way the
  // pair face decides what we are allowed to claim, and absence is decisive only
  // once it has ANSWERED.
  if (pairFacts.isPending || pairFacts.isFetching) return { status: "loading" };
  if (pairFacts.isError) return { status: "unresolved" };
  // Both faces settled without a row. Only a 404 — an actual answer — licenses
  // the claim about existence; a failed workspace read leaves us with nothing
  // proven, so it must not borrow the pair list's completeness contract.
  return workspaceFace.status === "not_found" ? { status: "not_found" } : { status: "unresolved" };
}

/**
 * The carrier: both faces for one id, composed by {@link resolveConversationIdentity}.
 *
 * The workspace face is DISABLED for an id the viewer's own pair list already
 * names — that request can only 404, and a chip in a busy stream would issue one
 * per private line. The pair list itself costs nothing per reader: it is one
 * viewer-global entry and mounting an observer on it never triggers a re-read
 * (`DIRECT_PAIRS_QUERY_POLICY` — the invalidation ladder is
 * `useDirectPairsSync` + the first-send mutation, not observer mounts), so with
 * it in hand a DM reference resolves with no request at all. A cold list still
 * costs one 404 on the workspace face, which is that face's correct answer and is
 * cached per id with retry off.
 *
 * `id === null` reads as `loading`, matching {@link useConversationById}: the
 * caller has nothing to resolve yet and no surface renders a state for it.
 */
export function useConversationIdentity(id: string | null): ConversationIdentityResolution {
  const pairs = useDirectPairsList();
  const rows = pairs.data?.direct_pairs;
  const heldPair =
    id !== null && rows !== undefined ? rows.some((p) => p.conversation_id === id) : false;
  const workspaceFace = useConversationById(heldPair ? null : id);
  if (id === null) return { status: "loading" };
  return resolveConversationIdentity(id, workspaceFace, {
    rows,
    isPending: pairs.isPending,
    isFetching: pairs.isFetching,
    isError: pairs.isError,
  });
}

/**
 * A resolved identity as the pane descriptor every chat pane consumes.
 *
 * The pair arm goes through `directPairPaneDescriptor` — the SAME builder the
 * primary DM pane uses — so a private line opened in the side pane wears the
 * counterpart's face, keeps its read-only state, and reports `workspace_id:
 * null` to every workspace-scoped affordance downstream. Nothing about the DM
 * chrome is re-derived per pane.
 */
export function conversationPaneDescriptorFromIdentity(
  identity: ConversationIdentity,
): ConversationPaneDescriptor {
  if (identity.placement === "direct_pair") {
    return directPairPaneDescriptor(
      identity.pair,
      presentCounterpart(identity.pair.counterpart).label,
    );
  }
  const ref = identity.ref;
  return {
    id: ref.id,
    title: ref.title,
    placement_kind: "workspace",
    roster_policy: ref.roster_policy,
    closed_at: ref.closed_at,
    archived_at: ref.archived_at,
    workspace_id: ref.workspace_id,
  };
}
