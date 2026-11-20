import type { AppSection } from "@/components/layout/nav-rail";
import type { MessageJumpTarget } from "@/features/channels/use-message-jump-controller";
import type { DirectPairAuthority } from "@/features/app-shell/chat-place-restore";
import {
  resolveJumpNav,
  resolveJumpRoute,
  jumpBasePath,
} from "@/features/conversation-shell/model/message-jump-nav";
import { paneUrl } from "@/navigation/pane-query";

export interface MessageJumpEnvironment {
  section: AppSection;
  pairs: DirectPairAuthority;
  resolveWorkspace: (workspaceId: string | undefined, conversationId: string) => string | null;
  prime: (target: MessageJumpTarget) => void;
  navigate: (url: string) => void;
}

export function executeMessageJump(
  target: MessageJumpTarget,
  env: MessageJumpEnvironment,
): boolean {
  if (target.preserveSidePanel && !target.threadRootId) {
    env.prime(target);
    return false;
  }

  const decision = resolveJumpNav({
    hasWorkspaceCoordinate: !!target.workspaceId,
    channelId: target.channelId,
    pairs: env.pairs,
  });
  if (decision === "fail-closed") {
    return false;
  }

  const route = resolveJumpRoute({
    decision,
    channelId: target.channelId,
    navWorkspaceId: env.resolveWorkspace(target.workspaceId, target.channelId),
    threadRootId: target.threadRootId,
  });
  if (!route) {
    return false;
  }

  env.prime(target);
  env.navigate(paneUrl(jumpBasePath(env.section, route), { thread: route.threadRootId }));
  return true;
}
