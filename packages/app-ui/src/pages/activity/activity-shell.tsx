import { useCompactLayout } from "@/navigation/layout";
import { SectionLayer } from "@/components/layout/section-layer";
import { ActivityView } from "@/pages/activity/activity-view";
import { VerticalSplitter } from "@/components/layout/vertical-splitter";
import { useResizableWidth } from "@/hooks/use-resizable-width";
import type { ActivityShellModel } from "@/features/conversation-shell/conversation-shell-models";
import type { ConversationContent } from "@/features/conversation-shell/conversation-content";
import { ConversationStage } from "@/features/conversation-shell/conversation-stage";

export function ActivityShell({
  model,
  content,
}: {
  model: ActivityShellModel;
  content: ConversationContent;
}) {
  const compact = useCompactLayout();
  const {
    appSection,
    selectedChannelId,
    agentsById,
    workspaceRefById,
    deferredSection,
    handleJumpToMessageStable,
  } = model;
  const activityList = useResizableWidth({
    storageKey: "silverhand.layout.activity-list-width",
    defaultWidth: 360,
    min: 280,
    max: 560,
    cssVar: "--activity-list-width",
  });

  return (
    <>
      {deferredSection === "activity" && (
        <SectionLayer visible={appSection !== "chat"}>
          {(!compact || selectedChannelId == null) && (
            <ActivityView
              width={selectedChannelId == null ? undefined : "var(--activity-list-width)"}
              workspaceRefById={workspaceRefById}
              agentsById={agentsById}
              onJump={handleJumpToMessageStable}
            />
          )}
          {selectedChannelId != null ? (
            <>
              {!compact && (
                <VerticalSplitter
                  onPointerDown={activityList.startResize}
                  active={activityList.isDragging}
                />
              )}
              <ConversationStage
                content={content}
                ready={appSection === "activity"}
                storageKey="silverhand.layout.activity-split-ratio"
              />
            </>
          ) : null}
        </SectionLayer>
      )}
    </>
  );
}
