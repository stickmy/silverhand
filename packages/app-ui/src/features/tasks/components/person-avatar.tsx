import { useMemo } from "react";
import { UserIcon } from "@/icons";
import type { AgentDto } from "@silverhand/shared";
import { AgentAvatar } from "@/components/agent-avatar";
import { UserAvatar } from "@/components/user-avatar";
import { AVATAR_PX } from "@/components/avatar-scale";
import { useAgents } from "@/features/agents/hooks/hooks";
import { useAgentAvatarObjectUrls } from "@/features/agents/hooks/avatar-hooks";
import { useUsersById } from "@/features/users/hooks";
import { useUserAvatarObjectUrls } from "@/features/users/avatar-hooks";
import type { PersonRef } from "@/features/tasks/model/task-filters";

/** The directory + resolved object URLs a {@link PersonAvatar} needs. Resolved
 *  ONCE per surface (see {@link usePersonDirectory}) and passed down, so a
 *  surface with several people on screen doesn't re-resolve per row. */
export interface PersonDirectory {
  agentsById: ReadonlyMap<string, AgentDto>;
  agentAvatarUrls: ReadonlyMap<string, string | null>;
  userAvatarUrls: ReadonlyMap<string, string | null>;
}

export function PersonAvatar({
  person,
  name,
  size = AVATAR_PX.xs,
  agentsById,
  agentAvatarUrls,
  userAvatarUrls,
}: {
  person: PersonRef;
  name: string;
  size?: number;
} & PersonDirectory): JSX.Element {
  if (person.type === "agent") {
    const runtime = agentsById.get(person.id)?.runtime;
    if (runtime) {
      return (
        <AgentAvatar
          runtime={runtime}
          size={size}
          avatarObjectUrl={agentAvatarUrls.get(person.id) ?? null}
          alt={name}
        />
      );
    }
    return <UserIcon className="size-4 shrink-0 text-text-muted" />;
  }
  return (
    <UserAvatar size={size} avatarObjectUrl={userAvatarUrls.get(person.id) ?? null} alt={name} />
  );
}

export function usePersonDirectory(): PersonDirectory {
  const { data: agents = [] } = useAgents();
  const agentsById = useMemo<ReadonlyMap<string, AgentDto>>(
    () => new Map(agents.map((a) => [a.id, a])),
    [agents],
  );
  const agentAvatarUrls = useAgentAvatarObjectUrls(agents);
  const usersById = useUsersById();
  const userList = useMemo(() => [...usersById.values()], [usersById]);
  const userAvatarUrls = useUserAvatarObjectUrls(userList);
  return { agentsById, agentAvatarUrls, userAvatarUrls };
}
