import { topSecondaryPane, type SecondaryPane } from "@/navigation/secondary-panes";

export type SecondarySurface = SecondaryPane | "none";

/** One presentation priority for rendering, read cursors and notification focus.
 * Lower-priority URL state can remain present as a return destination. */
export function selectSecondarySurface(input: {
  channelId: string | null;
  agentPanelId: string | null;
  humanPanelId: string | null;
  channelSettingsId?: string | null;
  attachmentPanelId: string | null;
  threadRootId: string | null;
  filesListMounted: boolean;
  taskPaneId: string | null;
  secondaryDescriptor: unknown;
  threadsListMounted: boolean;
}): SecondarySurface {
  const inChannel = Boolean(input.channelId);
  const top = topSecondaryPane({
    agent: Boolean(input.agentPanelId),
    human: Boolean(input.humanPanelId),
    settings: inChannel && Boolean(input.channelSettingsId),
    // Opened from the Attachments list, the preview renders inside `files`.
    attachment: inChannel && Boolean(input.attachmentPanelId) && !input.filesListMounted,
    files: inChannel && input.filesListMounted,
    task: inChannel && Boolean(input.taskPaneId),
    side: Boolean(input.secondaryDescriptor),
    thread: inChannel && Boolean(input.threadRootId),
    threads: inChannel && input.threadsListMounted,
  });
  return top ?? "none";
}
