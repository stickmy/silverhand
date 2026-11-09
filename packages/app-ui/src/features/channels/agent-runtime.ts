import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect } from "react";
import type {
  AgentRunStatesDto,
  ConversationAgentDto,
  ConversationAgentRuntimeDto,
} from "@silverhand/shared";
import { apiPaths } from "@silverhand/shared";
import { api } from "@/lib/api";
import { useInterest } from "@/features/realtime/interest-registry";
import {
  AGENT_RUN_STATES_GC_MS,
  agentRunStatesKey,
  reconcileAgentRunStateSnapshot,
} from "@/features/agents/model/agent-run-state-cache";
import { wsConversationSideScope } from "@/features/realtime/conversation-scope";
import { agentsKey, agentRuntimeKey } from "@/features/channels/message-cache-core";
import {
  mergePendingAgentStatusPatches,
  retainAgentStatusBuffer,
  releaseAgentStatusBuffer,
} from "@/features/channels/agent-status-cache";

/** Pure membership snapshot. Runtime updates belong to
 * {@link useChannelAgentRuntime}; this hook has no realtime interest because
 * process state cannot mutate a membership row. */
export function useChannelAgents(channelId: string | null) {
  const query = useQuery({
    queryKey: channelId ? agentsKey(channelId) : ["channels", "none", "agents"],
    enabled: !!channelId,
    queryFn: async () => {
      const res = await api<{ agents: ConversationAgentDto[] }>(apiPaths.channelAgents(channelId!));
      return res.agents;
    },
  });

  return query;
}

/** Scoped runtime projection for the channel's independently loaded roster. */
export function useChannelAgentRuntime(channelId: string | null) {
  const query = useQuery({
    queryKey: channelId ? agentRuntimeKey(channelId) : ["channels", "none", "agent-runtime"],
    enabled: !!channelId,
    queryFn: async () => {
      const res = await api<{ runtime: ConversationAgentRuntimeDto[] }>(
        apiPaths.channelAgentRuntime(channelId!),
      );
      return mergePendingAgentStatusPatches(channelId!, res.runtime);
    },
  });

  // Pure reader (#task:1255 P1c): no `onWsFrame` here. The central conversation
  // ingestor owns the live `agent_status` cache patch + the `snapshot_required`
  // reconcile (each gated on this reader). We keep the WS scope INTEREST alive
  // and retain the per-channel patch buffer, releasing on leave — the buffer is
  // cleared only when the LAST reader for the channel goes (refcounted) so a
  // concurrent reader's in-flight snapshot buffer isn't wiped.
  // #task:1260 P2: interest routes through the registry (`useInterest`) instead
  // of a direct subscribeWsScope useEffect. The refcounted patch buffer is NOT
  // interest — it stays its own effect on the same reader lifecycle (retain on
  // mount, release on unmount), untouched by the interest migration.
  useInterest(channelId ? wsConversationSideScope(channelId) : null);
  useEffect(() => {
    if (!channelId) return;
    retainAgentStatusBuffer(channelId);
    return () => releaseAgentStatusBuffer(channelId);
  }, [channelId]);

  return query;
}

/**
 * Ephemeral per-agent run states for the current channel.
 *
 * - Initial GET from the channel/thread run-state endpoint (snapshot on
 *   mount / reconnect — returns all currently in-flight states).
 * - Live `agent_activity` events are ingested by the central conversation
 *   ingestor (#task:1255 P1c), gated on this reader being mounted, so the cache
 *   stays warm across quick channel/thread navigation.
 */
export function useAgentRunStates(channelId: string | null) {
  const qc = useQueryClient();
  const key = channelId ? agentRunStatesKey(channelId) : ["channels", "none", "agent-run-states"];
  const query = useQuery({
    queryKey: key,
    enabled: !!channelId,
    queryFn: async () => {
      const requestedAt = Date.now();
      const path = apiPaths.channelAgentRunStates(channelId!);
      const res = await api<AgentRunStatesDto>(path);
      return reconcileAgentRunStateSnapshot(
        qc.getQueryData(channelId ? agentRunStatesKey(channelId) : key),
        res.states,
        requestedAt,
      );
    },
    // Don't stale-refetch automatically — realtime events keep this up-to-date.
    staleTime: Infinity,
    // Returning to a channel/thread after unsubscribing can miss terminal
    // activity events; always reconcile against the backend snapshot on mount.
    refetchOnMount: "always",
    // Keep recent activity visible across quick channel/thread navigation.
    gcTime: AGENT_RUN_STATES_GC_MS,
  });

  // Pure reader (#task:1255 P1c): no `onWsFrame` here. The central conversation
  // ingestor owns both the live `agent_activity` cache write
  // (`applyAgentRunStateToCache`) and the `snapshot_required` reconcile
  // (`invalidateAgentRunStates`), each gated on this reader being mounted. We
  // keep only the WS scope INTEREST alive so the transport stays subscribed.
  // #task:1260 P2: interest routes through the registry (`useInterest`) instead
  // of a direct subscribeWsScope useEffect — the last conversation reader in this
  // file to migrate, so hooks.ts no longer calls the transport subscribe
  // directly at all.
  useInterest(channelId ? wsConversationSideScope(channelId) : null);

  return query;
}
