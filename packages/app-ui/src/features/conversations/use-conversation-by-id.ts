import { useQuery, type UseQueryResult } from "@tanstack/react-query";
import { type ConversationRef } from "@silverhand/shared";
import { ApiError } from "@/lib/api";
import {
  conversationRefQueryOptions,
  type ConversationRefResponse,
} from "./conversation-ref-query";

export type ConversationByIdStatus =
  | "loading"
  | "resolved"
  | "unauthorized"
  | "not_found"
  /** The read settled in a failure that proves NOTHING about the id — a 5xx, a
   *  transport failure, an unparseable body — with the bounded retry budget
   *  spent. Terminal until something re-triggers the read. Distinct from
   *  `not_found` (a claim about existence this failure cannot support) and from
   *  `loading` (which would be a skeleton with nothing behind it). */
  | "unresolved";

export interface ConversationByIdResult {
  status: ConversationByIdStatus;
  /** Present iff `status === 'resolved'`. */
  ref: ConversationRef | null;
}

/** The id passed to the shared read point while the hook is disabled. Kept
 *  inside the factory (rather than a hand-written key) so this module never
 *  restates the conversation-ref key shape. */
const NO_ID = "none";

/**
 * Pure mapping from a React Query result for the conversation-ref fetch into the
 * {@link ConversationByIdResult} states exposed to the UI. Extracted so the
 * status mapping is unit-testable without a QueryClient + renderHook setup.
 *
 * Precedence, given that `isError` and `data` are NOT mutually exclusive (both
 * shapes below were measured on a real `QueryObserver`, @Cindy #task:2038):
 *   1. 403 / 404 are AUTHORITATIVE ANSWERS about the id and stay terminal even
 *      with a retained ref — the conversation is gone, or refused. The asymmetry
 *      is the point (@Alice): a 403/404 is a STATEMENT about this id and may
 *      overturn what we hold; a 5xx / transport failure states nothing and may
 *      not.
 *   2. Otherwise a held successful ref WINS over a refetch that is in flight or
 *      that settled in failure. A single 500 / dropped socket must not flip an
 *      already-labelled chip or an open conversation pane from its real title to an
 *      error placeholder. This is the same rule the pair face applies to a held
 *      row in `conversation-identity.ts`; the two faces may not disagree on it.
 *   3. With no ref in hand, `isError` IS the settled signal, with no in-flight
 *      criterion needed: React Query resets a data-less query to `pending` for
 *      each new attempt, so `isError && !data` never means "an attempt is
 *      running". The bounded retry budget (`failureCount < 2`) is therefore
 *      spent — terminal `unresolved`, because a spent budget rendered as
 *      `loading` is a skeleton that never ends (@Cindy, #task:2038 B1).
 *
 * WHY NO `isFetching` HERE, and what would license bringing it back. The rule it
 * would serve (@Alice) is: an in-flight criterion may only DELAY a verdict while
 * a request is genuinely in flight, never turn a settled failure into a
 * non-failure. Under measured semantics that rule has no work to do in case 3 —
 * the state it would fire on does not occur — so the branch would be unreachable
 * code. `use-conversation-by-id.test.ts` measures that reachability against the
 * production options, so if React Query ever retains `error` across a data-less
 * refetch (or `placeholderData` lands here, making `data` present with no
 * successful read behind it), that test goes red instead of this file quietly
 * reporting `unresolved` about a request that is still running.
 */
export function mapConversationByIdResult(
  id: string | null,
  query: Pick<UseQueryResult<ConversationRefResponse, Error>, "data" | "error" | "isError">,
): ConversationByIdResult {
  if (!id) return { status: "loading", ref: null };
  if (query.isError) {
    const err = query.error;
    if (err instanceof ApiError) {
      if (err.status === 404) return { status: "not_found", ref: null };
      if (err.status === 403) return { status: "unauthorized", ref: null };
    }
    // A failed refetch on top of a ref WE ALREADY HOLD proves nothing about the
    // id, so it must not throw the answer away. `isError` and `data` are not
    // mutually exclusive: a background refetch failure settles as
    // `isError: true, data: <held>` (@Cindy, #task:2038 — measured on a real
    // QueryObserver, see the reachability cases in the test).
    if (query.data) return { status: "resolved", ref: query.data.ref };
    return { status: "unresolved", ref: null };
  }
  if (query.data) return { status: "resolved", ref: query.data.ref };
  return { status: "loading", ref: null };
}

/**
 * Resolve a conversation by id (channel / dm_channel / thread) so the
 * conversation pane can render a conversation that lives in a different
 * workspace than the primary pane.
 *
 * Status mapping (see `mapConversationByIdResult`):
 *   - `loading`      → React Query is still fetching (initial load, or a
 *                       non-403/404 failure being retried) OR `id === null`.
 *   - `resolved`     → 200, `ref` populated — including a ref still in hand
 *                       while a later refetch is in flight or has failed.
 *   - `not_found`    → 404 from the backend. For a WORKSPACE-placed id that
 *                       means gone; for a `direct_pair` it is the correct
 *                       answer of an arm that cannot express one, which is why
 *                       this face is composed with the pair list rather than
 *                       read alone (`useConversationIdentity`, #task:2038).
 *   - `unauthorized` → 403 from the backend (caller can't see this
 *                       workspace). v1 user permission model never
 *                       returns 403, but we map it here so the
 *                       backend stays the source of truth.
 *   - `unresolved`   → the read settled in failure with the retry budget spent
 *                       AND no ref in hand. Nothing was proven; consumers that
 *                       gate on `resolved` stay fail-closed by construction.
 */
export function useConversationById(id: string | null): ConversationByIdResult {
  const query = useQuery<ConversationRefResponse, Error>({
    ...conversationRefQueryOptions(id ?? NO_ID),
    enabled: !!id,
    // Don't auto-retry 403/404 — they are terminal states for this hook.
    retry: (failureCount, err) => {
      if (err instanceof ApiError) {
        if (err.status === 403 || err.status === 404) return false;
      }
      return failureCount < 2;
    },
  });

  return mapConversationByIdResult(id, query);
}
