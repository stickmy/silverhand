import type { DirectMessageTarget } from "./target";
import { useEffect } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import type {
  DirectPairListItem,
  DirectPairListResponse,
  MessageSendAck,
} from "@silverhand/shared";
import { apiPaths } from "@silverhand/shared";
import { api, apiUpload } from "@/lib/api";
import { isCurrentViewerScope, onWsFrame, subscribeViewerScope } from "@/lib/ws-client";

/** Query key for the viewer's direct-pair list. ONE cache entry: the list is
 *  viewer-global (like Activity and favorites) and takes no other coordinate. */
export const directPairsKey = ["direct-pairs"] as const;

export const DIRECT_PAIRS_QUERY_POLICY = {
  staleMs: Infinity,
} as const;

/**
 * Key + fetcher + freshness policy for the direct-pair list, as ONE value.
 *
 * Every reader spreads this rather than restating the key: a second `useQuery`
 * with the same key but its own `staleTime` would silently reintroduce the
 * observer-mount refetch this policy exists to remove (the first hook to mount
 * inside the stale window wins the entry, so the divergence would be invisible
 * from either call site).
 */
export function directPairsQueryOptions() {
  return {
    queryKey: directPairsKey,
    queryFn: ({ signal }: { signal?: AbortSignal }) =>
      api<DirectPairListResponse>(apiPaths.directPairs, { signal }),
    staleTime: DIRECT_PAIRS_QUERY_POLICY.staleMs,
  };
}

/**
 * Read the viewer's direct-pair list. A pure OBSERVER — it arms no WS
 * subscription, because the invalidation obligation belongs to the viewer's
 * session and not to whichever surface happens to be reading the list
 * ({@link useDirectPairsSync}). MANY surfaces read it (the sidebar section, App's
 * pane resolution, and — since #task:2038 — every `#conv:` chip and the conversation
 * pane), which is why mounting one must be free: {@link DIRECT_PAIRS_QUERY_POLICY}.
 */
export function useDirectPairsList() {
  return useQuery<DirectPairListResponse>(directPairsQueryOptions());
}

/** The rows, or an empty list while the query is cold — for consumers that render
 *  a list and have no separate empty/loading state to distinguish (the sidebar
 *  section renders nothing when there are no pairs, and "not loaded yet" looks the
 *  same). A consumer that must tell the two apart reads
 *  {@link useDirectPairsList} directly. */
export function useDirectPairs(): readonly DirectPairListItem[] {
  return useDirectPairsList().data?.direct_pairs ?? [];
}

export function findAgentDirectPair(
  pairs: readonly DirectPairListItem[],
  agentId: string,
): DirectPairListItem | null {
  return pairs.find((p) => p.counterpart.kind === "agent" && p.counterpart.id === agentId) ?? null;
}

/**
 * The facts the pre-mint draft pane's handoff decision rests on
 * ({@link decideDraftHandoff}). Named so the decision is a pure function instead
 * of conditions scattered through an effect.
 */
export interface DraftHandoffFacts {
  /** The pair the viewer turns out to already have with this agent, if the list
   *  now contains one — {@link findAgentDirectPair}. `null` while they have
   *  none, which is the whole point of the draft route. */
  existingPairId: string | null;
  /** The composer holds content the user typed (trimmed, non-empty). */
  draftDirty: boolean;
  /** A first send is in flight. */
  sendPending: boolean;
  /** The last send attempt settled in failure. */
  sendFailed: boolean;
}

/** Handoff verdict: leave the draft route for the real pair, or stay. */
export type DraftHandoffDecision = { kind: "stay" } | { kind: "handoff"; conversationId: string };

export function decideDraftHandoff(facts: DraftHandoffFacts): DraftHandoffDecision {
  if (facts.existingPairId === null) return { kind: "stay" };
  if (facts.sendPending || facts.sendFailed || facts.draftDirty) {
    return { kind: "stay" };
  }
  return { kind: "handoff", conversationId: facts.existingPairId };
}

/**
 * What the pane can know about the pair named by `/chat/d/:conversationId`.
 *
 * FIVE states — a row, plus the FOUR different things "no row" can mean, which
 * is two more than a boolean can carry:
 *
 *  - `ready` — the row is in hand; the pane can render.
 *  - `idle` — the route names no pair. Nothing is being awaited.
 *  - `resolving` — the CURRENT authority has not answered yet. This covers the
 *    cold first read AND a refetch over retained rows: on invalidation React
 *    Query keeps the previous `data` and raises `isFetching` while `isPending`
 *    stays false, so a viewer whose cached list predates the pair (exactly the
 *    acceptance case — a stranger's first DM) would otherwise read as a settled
 *    absence and flash the terminal empty state before the row arrives.
 *  - `absent` — the current authority answered and this viewer is not an
 *    endpoint of that conversation (or it does not exist). TERMINAL, and it gets
 *    its OWN rendering ({@link directPairPaneProps} `notFound`) rather than the
 *    no-selection empty state: a loader here would never end, and "select a
 *    channel" would claim the viewer chose nothing.
 *  - `error` — the read that would answer failed and left no row. Not `absent`
 *    (nothing was proven about the pair) and not `resolving` (nothing is in
 *    flight, and with `retry: 1` the client will not keep trying on its own).
 *
 * Precedence is `ready > resolving > error > absent`: a row we hold is enough to
 * render the pane even while a refetch is in flight, and only a settled,
 * successful read may claim absence.
 */
export type DirectPairPaneState =
  | { kind: "idle" }
  | { kind: "resolving" }
  | { kind: "ready"; pair: DirectPairListItem }
  | { kind: "error" }
  | { kind: "absent" };

/** The query facts {@link projectDirectPairPane} needs, named so the projection
 *  is a pure function of them and can be exercised without React or a socket.
 *  `rows: undefined` is "never answered"; an empty array is an ANSWER. */
export interface DirectPairListQueryFacts {
  rows: readonly DirectPairListItem[] | undefined;
  /** No data has ever been received for this key. */
  isPending: boolean;
  /** A fetch is in flight — true for the cold read AND for a refetch that is
   *  running on top of retained rows. */
  isFetching: boolean;
  /** The last fetch settled in failure. */
  isError: boolean;
}

/** Pure projection of (route id, query facts) → {@link DirectPairPaneState}. */
export function projectDirectPairPane(
  conversationId: string | null,
  facts: DirectPairListQueryFacts,
): DirectPairPaneState {
  if (conversationId === null) return { kind: "idle" };
  const pair = facts.rows?.find((p) => p.conversation_id === conversationId);
  if (pair) return { kind: "ready", pair };
  if (facts.isPending || facts.isFetching) return { kind: "resolving" };
  if (facts.isError) return { kind: "error" };
  return { kind: "absent" };
}

/**
 * The four questions the primary pane asks about the pair arm, as ONE value.
 *
 * This is the consumer half of {@link DirectPairPaneState} and it is a total,
 * injective mapping on purpose: a state the pane cannot express is a state that
 * only exists in this module's tests. `absent` in particular has to land
 * somewhere of its own — a settled "you are not an endpoint of that
 * conversation" rendered as the no-selection empty state would tell the viewer
 * they chose nothing while the route names a conversation and the authority has
 * already answered.
 */
export interface DirectPairPaneProps {
  /** The row to render, or null when there is nothing to render. */
  pair: DirectPairListItem | null;
  /** Wait: the current authority has not answered yet. */
  resolving: boolean;
  /** The read that would answer failed. */
  loadFailed: boolean;
  /** Answered, and this viewer is not an endpoint of that conversation. */
  notFound: boolean;
}

/**
 * Pure {@link DirectPairPaneState} → {@link DirectPairPaneProps}. Lives here
 * rather than inline in the shell so the five states map to five distinct
 * renderings by construction, and so collapsing any two of them (e.g. `absent`
 * into `idle`) changes an assertable value instead of silently changing only the
 * shipped UI.
 */
export function directPairPaneProps(state: DirectPairPaneState): DirectPairPaneProps {
  return {
    pair: state.kind === "ready" ? state.pair : null,
    resolving: state.kind === "resolving",
    loadFailed: state.kind === "error",
    notFound: state.kind === "absent",
  };
}

/**
 * The pane's carrier when the route is `/chat/d/:conversationId`. Thin wrapper
 * over {@link projectDirectPairPane} — the decision lives in the pure function so
 * the state machine is testable and every consumer reads the same one.
 */
export function useDirectPairPane(conversationId: string | null): DirectPairPaneState {
  const { data, isPending, isFetching, isError } = useDirectPairsList();
  return projectDirectPairPane(conversationId, {
    rows: data?.direct_pairs,
    isPending,
    isFetching,
    isError,
  });
}

/** One participant the DM task-assignee picker can offer. `isCurrent` marks the
 *  counterpart when it is ALREADY this task's assignee: it stays in the domain
 *  (so the picker always shows the DM's counterpart — shiki 2026-08-17) and is
 *  rendered selected / no-op, rather than being dropped, which was the
 *  empty-dropdown bug (a DM assigned to its one counterpart had its only candidate
 *  filtered out). */
export interface DirectPairAssigneeCandidate {
  type: "agent" | "user";
  id: string;
  /** The `@`-handle shown on the row. */
  name: string;
  /** The display name on the avatar. */
  displayName: string;
  /** Already the task's assignee → selected, picking it is a no-op. */
  isCurrent: boolean;
}

/** The DM assignee picker's view over the pair pane. `loading` / `error` are the
 *  non-answers that must NOT read as "no agent to assign"; only `ready` with an
 *  empty list is an authoritative "nobody assignable here". */
export type DirectPairAssigneeView =
  | { status: "loading"; candidates: readonly DirectPairAssigneeCandidate[] }
  | { status: "error"; candidates: readonly DirectPairAssigneeCandidate[] }
  | { status: "ready"; candidates: readonly DirectPairAssigneeCandidate[] };

export function projectDirectPairAssignee(
  pane: DirectPairPaneState,
  current: { type: "agent" | "user"; id: string } | null,
): DirectPairAssigneeView {
  switch (pane.kind) {
    case "idle":
    case "resolving":
      return { status: "loading", candidates: [] };
    case "error":
      return { status: "error", candidates: [] };
    case "absent":
      return { status: "ready", candidates: [] };
    case "ready": {
      const cp = pane.pair.counterpart;
      if (cp.status !== "active") {
        // A `deleted` counterpart is gone.
        return { status: "ready", candidates: [] };
      }
      const type = cp.kind === "agent" ? "agent" : "user";
      return {
        status: "ready",
        candidates: [
          {
            type,
            id: cp.id,
            name: cp.name,
            displayName: cp.display_name || cp.name,
            isCurrent: current?.type === type && current.id === cp.id,
          },
        ],
      };
    }
  }
}

/** Minimal WS frame shape {@link decideDirectPairsInvalidation} consumes, so the
 *  decision can be exercised without React or a socket (mirrors `ViewerSyncFrame`
 *  in `features/workspaces/hooks`). */
export interface DirectPairsSyncFrame {
  type: string;
  kind?: string;
}

/**
 * Pure decision: does this frame invalidate the direct-pair list?
 *
 * `snapshot_required` counts, for the same reason it counts on every other
 * viewer-scoped list: a replay gap may have swallowed the frame that announced a
 * brand-new pair, and the row that would be lost is one the client has no other
 * way to learn about.
 *
 * The caller gates `isCurrentViewerScope(frame.scope)` first — kept out of here so
 * the decision stays dependency-free (same split as
 * `decideViewerSyncInvalidations`).
 */
export function decideDirectPairsInvalidation(frame: DirectPairsSyncFrame): boolean {
  if (frame.type === "snapshot_required") return true;
  if (frame.type !== "event") return false;
  return frame.kind === "direct_pairs_changed";
}

/**
 * Keep the direct-pair list live. Mount ONCE post-auth
 * (`useAppShellGlobalSync`), NOT from the sidebar section that renders the list:
 * the case this whole feature exists for is a stranger's FIRST DM — an agent the
 * viewer never created and never messaged — and that row must appear immediately
 * even while the viewer is looking at another section, exactly like being added to
 * one's first workspace (`useViewerWorkspacesSync`).
 *
 * The carrier is the `viewer:<userId>` scope, which is subscribable with no
 * workspace and no open conversation. It has to be: a pair has no workspace scope
 * to listen on, and the conversation's own `message` frame reaches only clients
 * subscribed to that conversation — which a viewer who has never opened the line
 * cannot be.
 */
export function useDirectPairsSync(): void {
  const qc = useQueryClient();
  useEffect(() => {
    const sub = subscribeViewerScope();
    const unsub = onWsFrame((frame) => {
      if (
        (frame.type !== "event" && frame.type !== "snapshot_required") ||
        !isCurrentViewerScope(frame.scope)
      ) {
        return;
      }
      if (decideDirectPairsInvalidation(frame)) {
        void qc.invalidateQueries({ queryKey: directPairsKey });
      }
    });
    return () => {
      sub.unsubscribe();
      unsub();
    };
  }, [qc]);
}

/**
 * First-send door for a human-initiated human↔agent DirectPair (#task:2036
 * part ①). POSTs the draft's text to `POST /api/agents/:id/direct-messages`,
 * which is get-or-create: the pair is minted (and announced) the first time a
 * send commits and reused on every later send. The renderer does NOT allocate
 * the pair — opening or cancelling the draft has zero persistence, so there is
 * nothing to invalidate until a send actually returns.
 *
 * This is emphatically NOT a renderer copy of the backend's initiation
 * authority: the door re-authorizes every send (session scope, schema, content
 * cap, reply target) and its 403/404/400/413 are the authoritative answers. The
 * button's product-domain visibility (`session_scope === 'agent'`) narrows who
 * is OFFERED the door; it never stands in for that check.
 *
 * On success we invalidate `directPairsKey` so a freshly minted pair appears in
 * the DIRECT MESSAGES list even for a viewer whose cached list predates it; the
 * caller reads `ack.message.conversation_id` to navigate to the new line. The
 * caller retains one client_message_id while retrying the unedited draft;
 * the server binds it to the authenticated human and admitted request.
 */
export function useInitiateDirectMessage(target: DirectMessageTarget) {
  const qc = useQueryClient();
  return useMutation<
    MessageSendAck,
    Error,
    {
      content: string;
      client_message_id: string;
      attachments?: ReadonlyArray<{ file: File; label: string }>;
    }
  >({
    mutationFn: async ({ content, client_message_id, attachments = [] }) => {
      const path =
        target.kind === "agent"
          ? apiPaths.agentDirectMessages(target.id)
          : apiPaths.userDirectMessages(target.id);
      if (attachments.length === 0) {
        return api<MessageSendAck>(path, {
          method: "POST",
          body: JSON.stringify({ content, client_message_id }),
        });
      }
      // The pair the files belong to does not exist yet, so they travel with
      // the first message and the server stores them once it opens the pair.
      const form = new FormData();
      form.append(
        "payload",
        JSON.stringify({
          content,
          client_message_id,
          attachment_labels: attachments.map((item) => item.label),
        }),
      );
      for (const item of attachments) form.append("file", item.file);
      return apiUpload<MessageSendAck>(path, form);
    },
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: directPairsKey });
    },
  });
}
