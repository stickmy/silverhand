import { useEffect, useMemo } from "react";
import { useQuery, useQueryClient, type UseQueryResult } from "@tanstack/react-query";
import {
  apiPaths,
  type ConversationMembersReadResponse,
  type ConversationMembersResponse,
  type MemberDto,
} from "@silverhand/shared";
import { ApiError, api } from "@/lib/api";
import { onWsFrame } from "@/lib/ws-client";
import { useInterest } from "@/features/realtime/interest-registry";
import { wsConversationSideScope } from "@/features/realtime/conversation-scope";

export type ConversationMembersStatus = "loading" | "resolved" | "unauthorized" | "not_found";

export interface ConversationMembersResult {
  status: ConversationMembersStatus;
  /** Present iff `status === 'resolved'`. Empty array is a valid resolution. */
  members: MemberDto[];
}

/** What {@link useConversationMembers} returns: the roster plus this viewer's
 *  per-member removability. The roster shape stays its own type because the
 *  4-state mapping is roster-only — removability rides the READ envelope and is
 *  never derived from an error state. */
export interface ConversationMembersView extends ConversationMembersResult {
  removability: MemberRemovability;
}

/** This viewer's per-member removability, keyed by {@link memberDtoKey}
 *  (`kind:primary_key`). Absent key ⇒ NOT removable: the carrier is advisory
 *  display data (the DELETE routes stay authoritative), so it fail-closes and a
 *  surface that hides the action can never invent one. */
export type MemberRemovability = ReadonlyMap<string, boolean>;

const NO_REMOVABILITY: MemberRemovability = new Map();

/** Identity of a member across the two arrays of the members envelope:
 *  `members` (MemberDto) and `member_removability` (kind + primary key). Look
 *  removability up by this key, NEVER by index — an optimistically filtered
 *  `members` cache holds a shorter array than its carrier (#task:1688). */
export function memberDtoKey(member: MemberDto): string {
  return member.kind === "agent" ? `agent:${member.agent_id}` : `human:${member.user_id}`;
}

export function canRemoveMember(removability: MemberRemovability, member: MemberDto): boolean {
  return removability.get(memberDtoKey(member)) === true;
}

function buildRemovability(data: ConversationMembersReadResponse | undefined): MemberRemovability {
  // A response without the carrier (older backend) resolves to an empty map →
  // every row fail-closes to "not removable".
  if (!data?.member_removability) return NO_REMOVABILITY;
  const m = new Map<string, boolean>();
  for (const entry of data.member_removability) {
    m.set(`${entry.kind}:${entry.id}`, entry.can_remove);
  }
  return m;
}

const MEMBERS_GC_MS = 5 * 60 * 1000;
const MEMBERS_STALE_MS = 30 * 1000;

const conversationMembersKey = (id: string) => ["conversation-members", id] as const;

/** Prefix of every conversation's roster key. Exported for the one thing a
 *  per-conversation key can't express: a profile write that changes a human's
 *  denormalized identity (`display_name` / `avatar_url`, which the backend
 *  re-derives from the users table on every read) has to reseat EVERY roster
 *  this session has cached, not just the visible channel's (#task:1701). */
export const CONVERSATION_MEMBERS_KEY_PREFIX = ["conversation-members"] as const;

/**
 * Pure mapping from a React Query result into the renderer-facing
 * 4-state shape. Mirrors `mapConversationByIdResult` so the UI never
 * has to branch on raw error fields.
 *
 * Non-403/404 errors fall through to `loading`; the hook's `retry`
 * policy keeps trying those, so `loading` is honest.
 */
// One shared empty list, so a pending or failed read keeps a stable identity and
// does not invalidate what consumers memoize on `members`.
const NO_MEMBERS: MemberDto[] = [];

export function mapConversationMembersResult(
  id: string | null,
  query: Pick<UseQueryResult<ConversationMembersResponse, Error>, "data" | "error" | "isError">,
): ConversationMembersResult {
  if (!id) return { status: "loading", members: NO_MEMBERS };
  if (query.isError) {
    const err = query.error;
    if (err instanceof ApiError) {
      if (err.status === 404) return { status: "not_found", members: NO_MEMBERS };
      if (err.status === 403) return { status: "unauthorized", members: NO_MEMBERS };
    }
    return { status: "loading", members: NO_MEMBERS };
  }
  if (query.data) return { status: "resolved", members: query.data.members };
  return { status: "loading", members: NO_MEMBERS };
}

/**
 * Fetch the resolved member list (agents + humans) for a conversation
 * by id. Backs the chat-header member-chip strip (PR-C). Called once
 * per opened conversation; React Query caches per id so channel ↔
 * thread navigation does not refetch within `staleTime`.
 *
 * The backend route is the single source of truth shared with the
 * platform prompt's "Channel members:" block — divergence
 * between what the agent talks about and what the chip strip shows is
 * structurally impossible.
 *
 * Self filtering is the caller's job: the renderer drops the current
 * human viewer (UI-self) by `kind === 'human' && user_id === viewer.id`.
 * This hook does not filter — it returns the full list so other
 * surfaces (popovers, "+N more" overlays) can render every member.
 *
 * Realtime invalidation: subscribes to the side-channel scope
 * `conversation-side:{id}` (#task:1255 P2 step-3 — `conversation_members_changed`
 * is a side kind) and invalidates the cache on `conversation_members_changed`.
 * This means
 * the renderer reseats the strip when a member is added by ANY route
 * — the local renderer mutation, an agent's `silverhand member add`,
 * or another window doing the same. `snapshot_required` for the same
 * scope also triggers a refetch (epoch mismatch / replay gap).
 */
export function useConversationMembers(id: string | null): ConversationMembersView {
  const qc = useQueryClient();
  const query = useQuery<ConversationMembersReadResponse, Error>({
    queryKey: id ? conversationMembersKey(id) : ["conversation-members", "none"],
    enabled: !!id,
    staleTime: MEMBERS_STALE_MS,
    gcTime: MEMBERS_GC_MS,
    queryFn: () => api<ConversationMembersReadResponse>(apiPaths.conversationMembers(id!)),
    // Don't auto-retry 403/404 — terminal for this hook.
    retry: (failureCount, err) => {
      if (err instanceof ApiError) {
        if (err.status === 403 || err.status === 404) return false;
      }
      return failureCount < 2;
    },
  });

  // #task:1260 P2 (distinct slice): scope INTEREST is migrated to the registry.
  // The `conversation_members_changed` LISTENER below is this hook's OWN concern
  // (not moved to the central ingestor in #1255) and stays verbatim —
  // interest ⊥ listener: only the direct subscribe/unsubscribe leaves the effect.
  useInterest(id ? wsConversationSideScope(id) : null);

  useEffect(() => {
    if (!id) return;
    // `conversation_members_changed` is a side-channel kind (#task:1255 P2
    // step-3) — match on the side scope, not the message stream.
    const scope = wsConversationSideScope(id);
    const unsub = onWsFrame((frame) => {
      if (frame.type === "event") {
        if (frame.scope !== scope) return;
        if (frame.kind !== "conversation_members_changed") return;
        void qc.invalidateQueries({ queryKey: conversationMembersKey(id) });
      } else if (frame.type === "snapshot_required" && frame.scope === scope) {
        void qc.invalidateQueries({ queryKey: conversationMembersKey(id) });
      }
    });
    return () => {
      unsub();
    };
  }, [id, qc]);

  const roster = mapConversationMembersResult(id, query);
  // Memoized on the cached response so the map identity is stable across
  // renders (consumers key memos off it).
  const removability = useMemo(() => buildRemovability(query.data), [query.data]);
  return {
    ...roster,
    // Only a resolved roster carries removability; an errored/loading read
    // fail-closes every row.
    removability: roster.status === "resolved" ? removability : NO_REMOVABILITY,
  };
}
