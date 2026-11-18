import { AnimatePresence, motion, useReducedMotion } from "motion/react";
import {
  PAGE_SWAP_EXIT_S,
  PageSwapTransition,
  pageSwapVariants,
} from "@/components/layout/page-swap";
import { ChannelSettingsPanel } from "./components/channel-settings-panel";
import { HumanView } from "@/features/profile/human-view";
import { ChannelView } from "@/features/conversation-shell/components/channel-view";
import { ThreadPanel } from "@/features/conversation-shell/components/thread-panel";
import { ChannelThreadsPanel } from "@/features/conversation-shell/components/channel-threads-panel";
import { ChannelAttachmentsPanel } from "@/features/conversation-shell/components/channel-attachments-panel";
import { AttachmentSidePanel } from "@/features/conversation-shell/components/attachment-side-panel";
import { TaskDetailPanel } from "@/features/tasks/components/task-detail-panel";
import { AgentView } from "@/features/agents/components/agent-view";
import { SideUnresolvedPlaceholder } from "@/features/conversation-shell/components/side-unresolved-placeholder";
import { ConversationPaneCapabilitiesProvider } from "@/features/conversations/pane-capabilities-context";
import type { AttachmentPaneProps, FilesPaneProps, SecondaryPaneModel } from "./pane-render-models";

export function SecondaryPaneContent({ model }: { model: SecondaryPaneModel }) {
  switch (model.kind) {
    case "none":
      return null;
    case "agent":
      return <AgentView mode="side" key={`side-${model.props.agentId}`} {...model.props} />;
    case "human":
      return <HumanView variant="side" key={`human-${model.props.userId}`} {...model.props} />;
    case "settings":
      return (
        <ChannelSettingsPanel
          key={model.channelId}
          channelId={model.channelId}
          onDepartChannel={model.onDepartChannel}
        />
      );
    case "files":
      return <FilesPane list={model.props} preview={model.preview} />;
    case "attachment":
      return <AttachmentSidePanel key={model.props.attachmentId} {...model.props} />;
    case "task":
      return <TaskDetailPanel key={`task-${model.props.taskId}`} {...model.props} />;
    case "conversation":
      return model.status === "resolved" ? (
        <ChannelView {...model.props} />
      ) : (
        <SideUnresolvedPlaceholder {...model.props} />
      );
    case "thread":
      // A thread inherits the parent's capability; it has no placement of its own.
      return (
        <ConversationPaneCapabilitiesProvider value={model.capabilities}>
          <ThreadPanel {...model.props} />
        </ConversationPaneCapabilitiesProvider>
      );
    case "threads":
      return <ChannelThreadsPanel {...model.props} />;
  }
}

/** The list stays laid out under a preview (hidden, not unmounted or
 * display:none) so Back returns to the same scroll position. */
function FilesPane({
  list,
  preview,
}: {
  list: FilesPaneProps;
  preview: AttachmentPaneProps | null;
}): JSX.Element {
  const reducedMotion = useReducedMotion();

  return (
    <div className="relative h-full min-h-0 w-full">
      <motion.div
        ref={(node) => node?.toggleAttribute("inert", preview !== null)}
        className="h-full"
        custom={1}
        variants={pageSwapVariants(!!reducedMotion, PAGE_SWAP_EXIT_S)}
        initial={false}
        animate={preview ? "exit" : "visible"}
      >
        <ChannelAttachmentsPanel {...list} />
      </motion.div>
      <AnimatePresence initial={false} custom={preview ? 1 : -1}>
        {preview ? (
          <PageSwapTransition
            key={preview.attachmentId}
            direction={1}
            enterDelay={PAGE_SWAP_EXIT_S}
            className="absolute inset-0"
          >
            <AttachmentSidePanel {...preview} />
          </PageSwapTransition>
        ) : null}
      </AnimatePresence>
    </div>
  );
}
