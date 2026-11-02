import { beforeEach, describe, expect, it, vi } from "vitest";
import { MutationObserver, QueryClient, QueryObserver } from "@tanstack/react-query";
import type {
  AgentRuntimeSeatDto,
  AgentRuntimeStateDto,
  DirectPairListItem,
  UserDto,
} from "@silverhand/shared";
import { toast } from "@/lib/toast";
import { api } from "@/lib/api";
import { useClearAgentSession, useStartAgent, useStopAgent } from "@/features/channels/hooks";
import { agentRuntimeStateKey, useAgentRuntimeState } from "../hooks/hooks";
import { canControlDirectAgent, directAgentControlState } from "./direct-agent-control";
const mocks = vi.hoisted(() => ({ client: null as unknown as QueryClient }));
vi.mock("@/lib/api", async (orig) => ({
  ...(await orig<typeof import("@/lib/api")>()),
  api: vi.fn(),
}));
vi.mock("@/lib/toast", () => ({ toast: { error: vi.fn() } }));
vi.mock("@tanstack/react-query", async (orig) => {
  const original = await orig<typeof import("@tanstack/react-query")>();
  return {
    ...original,
    useQueryClient: () => mocks.client,
    useMutation: (options: ConstructorParameters<typeof MutationObserver>[1]) => {
      const observer = new original.MutationObserver(mocks.client, options);
      return { mutateAsync: observer.mutate.bind(observer) };
    },
    useQuery: (options: ConstructorParameters<typeof QueryObserver>[1]) =>
      new original.QueryObserver(mocks.client, options),
  };
});
const agent = { id: "agent-b", created_by_user_id: "owner", session_scope: "agent" as const };
const viewer = { id: "owner" } as UserDto;
const pair: DirectPairListItem = {
  conversation_id: "dm-b",
  read_only: false,
  unread_count: 0,
  created_at: 0,
  counterpart: {
    status: "active",
    kind: "agent",
    id: agent.id,
    name: "b",
    display_name: "B",
    avatar_url: null,
    runtime: "codex",
  },
};
const seat = (status: AgentRuntimeSeatDto["status"]): AgentRuntimeSeatDto => ({
  scope: { kind: "agent" },
  generation: 7,
  status,
  pid: 42,
  cli_session_id: null,
  focus: { kind: "other" },
  queue_depth: 0,
});
const state = (seats: AgentRuntimeSeatDto[]): AgentRuntimeStateDto => ({
  seats,
  participations: [],
  visible_participations: [],
});
beforeEach(() => {
  mocks.client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  vi.clearAllMocks();
});
describe("direct agent control admission", () => {
  it("requires creator and active counterpart in viewer-owned pair list", () => {
    expect(canControlDirectAgent(viewer, agent, pair)).toBe(true);
    expect(canControlDirectAgent(undefined, agent, pair)).toBe(false);
    expect(canControlDirectAgent(viewer, agent, undefined)).toBe(false);
    expect(canControlDirectAgent(viewer, { ...agent, session_scope: "conversation" }, pair)).toBe(
      false,
    );
    expect(canControlDirectAgent(viewer, agent, { ...pair, read_only: true })).toBe(false);
    expect(
      canControlDirectAgent(viewer, agent, {
        ...pair,
        counterpart: { status: "deleted", kind: "agent", id: agent.id },
      }),
    ).toBe(false);
    expect(
      canControlDirectAgent(viewer, agent, {
        ...pair,
        counterpart: { ...pair.counterpart, id: "other" },
      }),
    ).toBe(false);
  });
  it("does not grant non-creator controls from readable runtime", () => {
    expect(directAgentControlState(state([seat("active")]), "online").stoppable).toBe(true);
    expect(canControlDirectAgent({ ...viewer, id: "other-human" }, agent, pair)).toBe(false);
  });
  it("uses global seat regardless of focus, respects unknown/host readiness", () => {
    expect(directAgentControlState(undefined, "online").ready).toBe(false);
    expect(directAgentControlState(state([]), "unknown").ready).toBe(false);
    expect(directAgentControlState(state([]), "offline").ready).toBe(false);
    expect(directAgentControlState(state([]), "online")).toEqual({
      ready: true,
      startable: true,
      stoppable: false,
    });
    expect(
      directAgentControlState(
        state([
          { ...seat("active"), scope: { kind: "conversation", conversation_id: "dm-b" } },
          seat("idle"),
        ]),
        "online",
      ).stoppable,
    ).toBe(false);
    expect(directAgentControlState(state([seat("starting")]), "online")).toEqual({
      ready: true,
      startable: false,
      stoppable: true,
    });
    expect(directAgentControlState(state([seat("crashed")]), "online").startable).toBe(true);
  });
});
describe("direct agent control mutations", () => {
  it("posts actual DM/agent with empty body and rereads global seat", async () => {
    let runtime = state([seat("idle")]);
    vi.mocked(api).mockImplementation(async (path, options) => {
      if (path === "/api/agents/agent-b/runtime-state") return runtime as never;
      expect(options).toEqual({ method: "POST" });
      if (path === "/api/channels/dm-b/agents/agent-b/start") runtime = state([seat("active")]);
      else if (path === "/api/agents/agent-b/clear-session") {
        runtime = state([seat("idle")]);
        return { ok: true, prev_cli_session_id: "old-session", was_running: true } as never;
      } else if (path === "/api/channels/dm-b/agents/agent-b/stop") runtime = state([seat("idle")]);
      else throw new Error(`Unexpected target ${path}`);
      return { ok: true } as never;
    });
    const observer = useAgentRuntimeState(
      agent.id,
    ) as unknown as QueryObserver<AgentRuntimeStateDto>;
    const unsub = observer.subscribe(() => {});
    await observer.refetch();
    mocks.client.setQueryData(agentRuntimeStateKey("other"), state([]));
    await useStartAgent(pair.conversation_id).mutateAsync(agent.id);
    await vi.waitFor(() =>
      expect(observer.getCurrentResult().data?.seats[0]?.status).toBe("active"),
    );
    await useStopAgent(pair.conversation_id).mutateAsync({ agentId: agent.id });
    await vi.waitFor(() => expect(observer.getCurrentResult().data?.seats[0]?.status).toBe("idle"));
    await useStartAgent(pair.conversation_id).mutateAsync(agent.id);
    await useClearAgentSession(pair.conversation_id, "agent").mutateAsync(agent.id);
    await vi.waitFor(() => expect(observer.getCurrentResult().data?.seats[0]?.status).toBe("idle"));
    expect(mocks.client.getQueryState(agentRuntimeStateKey("other"))?.isInvalidated).toBe(false);
    expect(
      vi
        .mocked(api)
        .mock.calls.filter(([, options]) => options?.method === "POST")
        .map(([path]) => path),
    ).toEqual([
      "/api/channels/dm-b/agents/agent-b/start",
      "/api/channels/dm-b/agents/agent-b/stop",
      "/api/channels/dm-b/agents/agent-b/start",
      "/api/agents/agent-b/clear-session",
    ]);
    unsub();
    mocks.client.clear();
  });
  it("reports failures without claiming state update", async () => {
    const runtime = state([seat("active")]);
    mocks.client.setQueryData(agentRuntimeStateKey(agent.id), runtime);
    vi.mocked(api).mockRejectedValue(new Error("denied"));
    await expect(useStartAgent("dm-b").mutateAsync(agent.id)).rejects.toThrow("denied");
    await expect(useStopAgent("dm-b").mutateAsync({ agentId: agent.id })).rejects.toThrow("denied");
    await expect(useClearAgentSession("dm-b", "agent").mutateAsync(agent.id)).rejects.toThrow(
      "denied",
    );
    expect(toast.error).toHaveBeenCalledTimes(3);
    expect(mocks.client.getQueryData(agentRuntimeStateKey(agent.id))).toEqual(runtime);
    expect(mocks.client.getQueryState(agentRuntimeStateKey(agent.id))?.isInvalidated).toBe(false);
    mocks.client.clear();
  });
});
