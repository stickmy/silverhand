import { useLayoutEffect, useRef, type ReactNode } from "react";
import { AnimatePresence, motion, useReducedMotion } from "motion/react";
import {
  PAGE_SWAP_EXIT_S,
  PageSwapTransition,
  pageSwapVariants,
} from "@/components/layout/page-swap";
import { paneOwner, type Pane } from "@/navigation/panes";
import { ChannelSettingsPanel } from "./components/channel-settings-panel";
import { HumanView } from "@/features/profile/human-view";
import { ThreadPanel } from "@/features/conversation-shell/components/thread-panel";
import { ChannelThreadsPanel } from "@/features/conversation-shell/components/channel-threads-panel";
import { ChannelAttachmentsPanel } from "@/features/conversation-shell/components/channel-attachments-panel";
import { AttachmentSidePanel } from "@/features/conversation-shell/components/attachment-side-panel";
import { TaskDetailPanel } from "@/features/tasks/components/task-detail-panel";
import { AgentView } from "@/features/agents/components/agent-view";
import { ConversationPaneCapabilitiesProvider } from "@/features/conversations/pane-capabilities-context";
import { buildAttachedPane, type PaneContext } from "./build-conversation-panes";
import type { AttachedPaneModel } from "./pane-render-models";

/** One attached pane; a conversation-owned pane belongs to the route's. */
export function AttachedPane({ pane, ctx }: { pane: Pane; ctx: PaneContext }): JSX.Element | null {
  const owned = paneOwner(pane) !== undefined;
  return (
    <AttachedPaneContent model={buildAttachedPane(pane, owned ? ctx.read.primary : null, ctx)} />
  );
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
      return (
        <DrillInPane
          list={<ChannelAttachmentsPanel {...model.props} />}
          detailKey={model.preview?.attachmentId ?? null}
          detail={model.preview ? <AttachmentSidePanel {...model.preview} /> : null}
        />
      );
    case "task":
      return <TaskDetailPanel key={`task-${model.props.taskId}`} {...model.props} />;
    case "threads":
      return (
        <DrillInPane
          list={<ChannelThreadsPanel {...model.props} />}
          detailKey={model.thread?.props.rootMessageId ?? null}
          detail={
            model.thread ? (
              // A thread inherits the parent's capability; it has no placement of its own.
              <ConversationPaneCapabilitiesProvider value={model.thread.capabilities}>
                <ThreadPanel {...model.thread.props} />
              </ConversationPaneCapabilitiesProvider>
            ) : null
          }
        />
      );
  }
}

/** A list that drills into one item. The list stays laid out under the
 * detail (hidden, not unmounted or display:none) so Back returns to the same
 * scroll position. */
function DrillInPane({
  list,
  detailKey,
  detail,
}: {
  list: ReactNode;
  detailKey: string | null;
  detail: ReactNode;
}): JSX.Element {
  const reducedMotion = useReducedMotion();
  const listRef = useRef<HTMLDivElement>(null);
  const open = detailKey !== null;

  // A motion element calls its ref only on mount, so the flag is synced here.
  useLayoutEffect(() => {
    listRef.current?.toggleAttribute("inert", open);
  }, [open]);

  return (
    <div className="relative h-full min-h-0 w-full">
      <motion.div
        ref={listRef}
        className="h-full"
        custom={1}
        variants={pageSwapVariants(!!reducedMotion, PAGE_SWAP_EXIT_S)}
        initial={false}
        animate={open ? "exit" : "visible"}
      >
        {list}
      </motion.div>
      <AnimatePresence initial={false} custom={open ? 1 : -1}>
        {open ? (
          <PageSwapTransition
            key={detailKey}
            direction={1}
            enterDelay={PAGE_SWAP_EXIT_S}
            className="absolute inset-0"
          >
            {detail}
          </PageSwapTransition>
        ) : null}
      </AnimatePresence>
    </div>
  );
}
