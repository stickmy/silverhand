import { useCompactLayout } from "@/navigation/layout";
import { useChatBrowse } from "./use-chat-browse";
import { useWorkspaceActions } from "./use-workspace-actions";
import { ChatBrowseSidebar } from "./chat-browse-sidebar";
import { WorkspaceActionDialogs } from "./workspace-action-dialogs";
import { SectionLayer } from "@/components/layout/section-layer";
import { SavedListView } from "@/pages/chat/saved-list-view";
import { SearchPanel } from "@/pages/chat/search-panel";
import { ArchivedChannelsPanel } from "@/pages/chat/archived-channels-panel";
import { SidebarSlot } from "@/components/layout/sidebar-slot";
import type { ChatShellModel } from "@/features/conversation-shell/conversation-shell-models";
import type { ConversationContent } from "@/features/conversation-shell/conversation-content";
import { ConversationStage } from "@/features/conversation-shell/conversation-stage";

export function isChatSidebarPage(model: ChatShellModel, content: ConversationContent): boolean {
  return (
    content.panes.length === 0 &&
    !model.route.channelId &&
    !model.directDraftTarget &&
    model.view === "channel"
  );
}

export function ChatShell({
  model,
  content,
}: {
  model: ChatShellModel;
  content: ConversationContent;
}) {
  const compact = useCompactLayout();
  const showList = compact && isChatSidebarPage(model, content);
  const {
    route,
    nav,
    appSection,
    view,
    selectedWorkspaceId,
    workspaces,
    resolveNavWorkspaceId,
    deferredSection,
    handleSelectWorkspace,
    handleJumpToMessage,
  } = model;

  const workspaceActions = useWorkspaceActions(workspaces, selectedWorkspaceId);

  const {
    savedFilter,
    setSavedFilter,
    searchFilter,
    setSearchFilter,
    archivedFilter,
    setArchivedFilter,
  } = useChatBrowse();

  return (
    <>
      {(appSection === "chat" || deferredSection === "chat") && (
        <SectionLayer visible>
          <>
            {(!compact || showList) && (
              <SidebarSlot fill={compact}>
                <ChatBrowseSidebar model={model} actions={workspaceActions} fill={compact} />
              </SidebarSlot>
            )}
            {!showList &&
              (view === "saved" ? (
                <SavedListView
                  workspaces={workspaces}
                  filter={savedFilter}
                  onFilterChange={setSavedFilter}
                  onJump={(params) => handleJumpToMessage(params)}
                />
              ) : view === "search" ? (
                <SearchPanel
                  workspaces={workspaces}
                  initialQuery={route.searchQuery ?? undefined}
                  filter={searchFilter}
                  onFilterChange={setSearchFilter}
                  onJumpMessage={(params) => handleJumpToMessage(params)}
                  onOpenWorkspace={(id) => nav.toWorkspace(id)}
                  onOpenConversation={(workspaceId, channelId) => {
                    const navWorkspaceId = resolveNavWorkspaceId(workspaceId, channelId);
                    if (!navWorkspaceId) {
                      return;
                    }
                    nav.toChannel(navWorkspaceId, channelId);
                  }}
                />
              ) : view === "archived" ? (
                <ArchivedChannelsPanel
                  workspaces={workspaces}
                  filter={archivedFilter}
                  onFilterChange={setArchivedFilter}
                  onOpenChannel={(workspaceId, channelId) => {
                    // Mirror SearchPanel: resolve through the same nav helper
                    // so a cross-workspace open lands on the right UI workspace
                    // item. For the archived aggregate `workspaceId` is already
                    // the channel's own workspace id (agent_home for a DM), so
                    // this is a no-op there — but it keeps both cross-workspace
                    // panels on one resolution path (#task:1145).
                    const navWorkspaceId = resolveNavWorkspaceId(workspaceId, channelId);
                    if (!navWorkspaceId) {
                      return;
                    }
                    nav.toChannel(navWorkspaceId, channelId);
                  }}
                  onOpenWorkspace={handleSelectWorkspace}
                  onOpenWorkspaceSettings={workspaceActions.setWorkspaceSettingsId}
                />
              ) : (
                <>
                  <ConversationStage
                    content={content}
                    ready={deferredSection === "chat"}
                    storageKey="silverhand.layout.split-ratio"
                  />
                </>
              ))}
          </>
        </SectionLayer>
      )}{" "}
      <WorkspaceActionDialogs actions={workspaceActions} />
    </>
  );
}
