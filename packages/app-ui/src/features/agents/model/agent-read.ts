import type { AgentDto, AgentPublicDto } from "@silverhand/shared";

/** True when the viewer read the full record. The public tier omits the
 *  restricted keys entirely, so their absence is the tier. */
export function isPrivilegedAgent(agent: AgentDto | AgentPublicDto): agent is AgentDto {
  return "session_scope" in agent;
}
