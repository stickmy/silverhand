import { agentRuntimeStateKey } from "@/features/agents/hooks/hooks";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";
import { t } from "@/lib/i18n";
import { clearAgentSessionPath } from "./runtime-control";
import type {
  AgentDto,
  ClearSessionResponse,
  ConversationAgentDto,
  ConversationMembersResponse,
} from "@silverhand/shared";
import { apiPaths } from "@silverhand/shared";
import { api } from "@/lib/api";
import { agentsKey } from "@/features/channels/message-cache-core";
import {
  formatStartAgentError,
  formatRemoveAgentError,
  formatRemoveHumanError,
} from "@/features/channels/message-errors";

export function useStartAgent(channelId: string | null) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (agentId: string) => {
      if (!channelId) throw new Error("no channel selected");
      const path = apiPaths.channelAgentStart(channelId, agentId);
      await api(path, {
        method: "POST",
      });
    },
    onSuccess: (_, agentId) => {
      void qc.invalidateQueries({ queryKey: agentRuntimeStateKey(agentId) });
    },
    onError: (err) => {
      toast.error(formatStartAgentError(err));
    },
  });
}

export function useStopAgent(channelId: string | null) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (params: { agentId: string }) => {
      if (!channelId) throw new Error("no channel selected");
      const path = apiPaths.channelAgentStop(channelId, params.agentId);
      await api(path, { method: "POST" });
    },
    onSuccess: (_, { agentId }) => {
      void qc.invalidateQueries({ queryKey: agentRuntimeStateKey(agentId) });
    },
    onError: () => {
      toast.error(t("channels.stopFailed"));
    },
  });
}

export function useClearAgentSession(
  channelId: string | null,
  sessionScope: AgentDto["session_scope"],
) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (agentId: string) => {
      const path = clearAgentSessionPath({
        agentId,
        sessionScope,
        conversationId: channelId,
      });
      return api<ClearSessionResponse>(path, { method: "POST" });
    },
    onSuccess: (res, agentId) => {
      void qc.invalidateQueries({ queryKey: agentRuntimeStateKey(agentId) });
      if (!channelId) return;
      qc.setQueryData<ConversationAgentDto[]>(agentsKey(channelId), (prev) =>
        prev
          ? prev.map((agent) =>
              agent.agent_id === agentId
                ? {
                    ...agent,
                    cli_session_id: null,
                    ...(res.was_running ? { status: "idle" as const, pid: null } : {}),
                  }
                : agent,
            )
          : prev,
      );
    },
    onError: () => {
      toast.error(t("channels.clearSessionFailed"));
    },
  });
}

/**
 * Remove an agent from THIS conversation only (human-triggered leave —
 * #task:957). The backend route `DELETE /api/conversations/:id/members/
 * agents/:agentId` (Cindy) stops the agent's runtime here, drops the
 * membership row, and emits a "member removed" system narration. The
 * path is placement-agnostic; the backend applies the roster-policy gate, so no
 * renderer-side conversation classifier is needed.
 *
 * Permission (agent owner OR workspace owner/admin) is enforced
 * server-side; the popover hides the control otherwise via
 * `canRemoveAgentFromChannel`, and this hook surfaces a toast if the
 * server still rejects (stale role, concurrent change).
 */
export function useRemoveAgentFromChannel(channelId: string | null) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (agentId: string) => {
      if (!channelId) throw new Error("no channel selected");
      await api(apiPaths.conversationAgentMember(channelId, agentId), {
        method: "DELETE",
      });
    },
    onSuccess: (_res, agentId) => {
      if (!channelId) return;
      // Drop the removed agent from the per-conversation roster cache so
      // the member strip / popover update immediately; the backend's
      // `conversation_members_changed` event reconciles other windows.
      qc.setQueryData<ConversationAgentDto[]>(agentsKey(channelId), (prev) =>
        prev ? prev.filter((a) => a.agent_id !== agentId) : prev,
      );
      // The header member-chip strip reads from a *separate* cache
      // (`['conversation-members', id]` via `useConversationMembers`), so
      // strip the removed agent there too — otherwise the chip lingers
      // until the invalidate refetch lands (visible on slow networks).
      // Invalidate stays as the reconciler against the authoritative list.
      qc.setQueryData<ConversationMembersResponse>(["conversation-members", channelId], (prev) =>
        prev
          ? {
              ...prev,
              members: prev.members.filter((m) => !(m.kind === "agent" && m.agent_id === agentId)),
            }
          : prev,
      );
      void qc.invalidateQueries({ queryKey: agentsKey(channelId) });
      void qc.invalidateQueries({
        queryKey: ["conversation-members", channelId],
      });
    },
    onError: (err) => {
      toast.error(formatRemoveAgentError(err));
    },
  });
}

/**
 * Remove ANOTHER human from this channel (#task:1687 UI / #task:1688 route) —
 * `DELETE /api/conversations/:id/members/users/:userId`. The third-party
 * counterpart of self-leave (`POST /channels/:id/leave`), which stays its own
 * verb with its own gating; this hook is never used for the viewer's own row.
 *
 * Permission (workspace admin ∧ the target does not outrank the caller ∧ the
 * channel's exit guards evaluated for the TARGET) is enforced server-side. The
 * members modal hides the control per the `member_removability` carrier, so a
 * rejection here means the carrier went stale — surfaced as a toast, same as the
 * agent path.
 */
export function useRemoveHumanFromChannel(channelId: string | null) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (userId: string) => {
      if (!channelId) throw new Error("no channel selected");
      await api(apiPaths.conversationHumanMember(channelId, userId), {
        method: "DELETE",
      });
    },
    onSuccess: (_res, userId) => {
      if (!channelId) return;
      // Drop the row from the roster cache the modal / member strip read, so the
      // list settles before the invalidate refetch lands. The stale
      // `member_removability` entry for the removed user is harmless: lookups
      // are keyed by `(kind, id)`, never by index.
      qc.setQueryData<ConversationMembersResponse>(["conversation-members", channelId], (prev) =>
        prev
          ? {
              ...prev,
              members: prev.members.filter((m) => !(m.kind === "human" && m.user_id === userId)),
            }
          : prev,
      );
      void qc.invalidateQueries({
        queryKey: ["conversation-members", channelId],
      });
    },
    onError: (err) => {
      toast.error(formatRemoveHumanError(err));
    },
  });
}
