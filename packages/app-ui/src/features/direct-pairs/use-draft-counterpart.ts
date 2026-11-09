import { useAgent } from "@/features/agents/hooks/hooks";
import { useUsers } from "@/features/users/hooks";
import type { DirectMessageTarget } from "./target";
import { counterpartFromAgent, counterpartFromHuman } from "./counterpart-presentation";

export function useDraftCounterpart(target: DirectMessageTarget | null) {
  const agent = useAgent(target?.kind === "agent" ? target.id : null);
  const users = useUsers({ enabled: target?.kind === "human" });
  const human =
    target?.kind === "human" ? users.data?.find((user) => user.id === target.id) : undefined;
  return {
    isLoading: target?.kind === "agent" ? agent.isLoading : users.isLoading,
    counterpart:
      target?.kind === "agent" && agent.data
        ? counterpartFromAgent(agent.data.agent)
        : human
          ? counterpartFromHuman(human)
          : null,
  };
}
