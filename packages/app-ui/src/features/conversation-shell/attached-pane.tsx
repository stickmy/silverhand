import { useEffect } from "react";
import { AnimatePresence, motion, useReducedMotion } from "motion/react";
import {
  PAGE_SWAP_EXIT_S,
  PageSwapTransition,
  pageSwapVariants,
} from "@/components/layout/page-swap";
import { paneOwner, type Pane } from "@/navigation/panes";
import { useMarkMessageChannelRead } from "@/features/channels/hooks";
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
import { PaneOwnerProvider } from "@/components/layout/side-panel-header";
import { useConversationColumn, type ConversationColumn } from "./conversation-column";
import { buildAttachedPane, type PaneContext } from "./build-conversation-panes";
import type { AttachedPaneModel, AttachmentPaneProps, FilesPaneProps } from "./pane-render-models";

/** One attached pane: resolves the conversation it belongs to, then renders. */
export function AttachedPane({ pane, ctx }: { pane: Pane; ctx: PaneContext }): JSX.Element | null {
  const { read, route } = ctx;
  const ownerId = pane.kind === "conversation" ? pane.id : (paneOwner(pane) ?? null);
  const own = ownerId !== null && ownerId === route.channelId;
  const other = useConversationColumn(
    own ? null : ownerId,
    read.channels,
    read.buildChannelDescriptor,
    read.workspaceNameById,
  );
  const owner = own ? read.primary : other;
  useReadWhileShown(pane.kind === "conversation" && owner?.descriptor ? owner.id : null);

  return (
    <PaneOwnerProvider value={own || !NAMES_OWNER.has(pane.kind) ? null : ownerLabel(other)}>
      <AttachedPaneContent model={buildAttachedPane(pane, owner, ctx)} />
    </PaneOwnerProvider>
  );
}

/** Panes whose header names their conversation when it is not the route's.
 *  A conversation pane is its own owner, and Settings already titles itself
 *  with the channel. */
const NAMES_OWNER = new Set<Pane["kind"]>(["threads", "files"]);

/** How a pane's header names the conversation it belongs to. */
function ownerLabel(owner: ConversationColumn | null): string | null {
  if (!owner?.descriptor) return null;
  return owner.paneTitle ? `#${owner.paneTitle}` : owner.descriptor.title;
}

/** A conversation pane on screen advances its read cursor once it resolves. */
function useReadWhileShown(conversationId: string | null): void {
  const markRead = useMarkMessageChannelRead();
  useEffect(() => {
    if (conversationId) markRead.mutate(conversationId);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [conversationId]);
}

function AttachedPaneContent({ model }: { model: AttachedPaneModel }) {
  switch (model.kind) {
    case "none":
      return null;
    case "agent":
      return <AgentView mode="side" key={`side-${model.props.agentId}`} {...model.props} />;
    case "human":
      return <HumanView variant="side" key={`human-${model.props.userId}`} {...model.props} />;
    case "settings":
      return <ChannelSettingsPanel key={model.props.channelId} {...model.props} />;
    case "files":
      return <FilesPane list={model.props} preview={model.preview} />;
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
