import { AgentAvatar } from "@/components/agent-avatar";
import { AVATAR_PX } from "@/components/avatar-scale";
import { UserAvatar } from "@/components/user-avatar";
import { useAgents } from "@/features/agents/hooks/hooks";
import { useAgentAvatarObjectUrl } from "@/features/agents/hooks/avatar-hooks";
import { useUserAvatarObjectUrl } from "@/features/users/avatar-hooks";

/** A person named by a task event (actor or assignee): small avatar + name.
 *  Shared by the activity line and the task's History. */
export type EventPersonRef =
  | { kind: "agent"; id: string; name: string }
  | { kind: "user"; id: string | null; name: string };

export function EventPerson({ person }: { person: EventPersonRef }): JSX.Element {
  return (
    <span data-task-event-person>
      {person.kind === "agent" ? (
        <EventAgentAvatar id={person.id} name={person.name} />
      ) : (
        <EventUserAvatar id={person.id} name={person.name} />
      )}
      <span data-task-event-actor>{person.name}</span>
    </span>
  );
}

function EventAgentAvatar({ id, name }: { id: string; name: string }): JSX.Element {
  const { data: agents } = useAgents();
  const agent = agents?.find((candidate) => candidate.id === id);
  const avatarObjectUrl = useAgentAvatarObjectUrl(agent?.avatar_url);

  if (!agent) {
    return <UserAvatar size={AVATAR_PX.chip} alt={name} />;
  }
  return (
    <AgentAvatar
      runtime={agent.runtime}
      size={AVATAR_PX.chip}
      avatarObjectUrl={avatarObjectUrl}
      alt={name}
    />
  );
}

function EventUserAvatar({ id, name }: { id: string | null; name: string }): JSX.Element {
  const avatarObjectUrl = useUserAvatarObjectUrl({ userId: id });

  return <UserAvatar size={AVATAR_PX.chip} avatarObjectUrl={avatarObjectUrl} alt={name} />;
}
