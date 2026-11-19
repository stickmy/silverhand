import type { ConversationPaneDescriptor } from "@/features/conversations/pane-descriptor";
import { projectConversationPaneMode } from "@/features/conversations/pane-mode";
import { taskBoardContainerForPane } from "@/features/tasks/model/task-container";

/** Whether a primary pane shows the view-tab strip: the viewer is a member and
 * the position forms a Tasks-board container. Writers into the tab store (the
 * side-panel Expand) read the same rule so they never add a tab nobody sees. */
export function hostsViewTabs(
  conversation: ConversationPaneDescriptor,
  allowMemberEffects: boolean,
): boolean {
  return (
    allowMemberEffects &&
    taskBoardContainerForPane(projectConversationPaneMode(conversation), {
      conversationId: conversation.id,
    }) !== null
  );
}
