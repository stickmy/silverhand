import { useMemo, type ReactElement } from "react";
import { SelectionToolbar } from "@/features/messages/components/selection-toolbar";
import { ImagePreviewModal } from "@/features/messages/components/image-preview-modal";
import { useAgentAvatarObjectUrls } from "@/features/agents/hooks/avatar-hooks";
import { collectCounterpartAvatarSources } from "@/features/direct-pairs/counterpart-presentation";
import { useUserAvatarObjectUrls } from "@/features/users/avatar-hooks";
import { collectAuthorAvatarSources } from "@/features/messages/model/author-presentation";
import type { MessageActionSurface } from "@/features/messages/hooks/use-message-action-surface";
import type { MessageRenderContext } from "@/features/messages/hooks/use-message-render-context";
import { useNativeSelectionBar } from "@/features/messages/hooks/use-native-selection-bar";
import type { WorkspaceChannelReference } from "./conversation-ref";
import type { ScreenshotHeader } from "./message-screenshot-view";

/** Render context the image-export modal needs: a subset of
 *  `MessageRenderContext` plus the agent avatar batch (lifted per surface
 *  so authors / member chips / activity strip share one fetch). */
export type MessageActionChromeRenderContext = Pick<
  MessageRenderContext,
  "agentsByName" | "humansByName" | "channelsByName" | "viewer" | "viewerAvatarObjectUrl"
> & {
  agentAvatarUrls: ReadonlyMap<string, string | null>;
  conversationRefsById?: ReadonlyMap<string, WorkspaceChannelReference>;
};

/**
 * The selection toolbar + image-export modal for the shared message action
 * surface (`useMessageActionSurface`). Both the channel pane and the thread
 * reply-cluster pane render this so the toolbar/modal wiring lives in one
 * place (#task:1067) and can't drift apart. Drop it where the toolbar
 * should sit — directly above the message list — in either pane.
 */
export function MessageActionChrome({
  surface,
  renderCtx,
  workspaceId,
  conversationId,
  selectionKey,
  header,
}: {
  surface: MessageActionSurface;
  renderCtx: MessageActionChromeRenderContext;
  workspaceId: string | null;
  /** The conversation these messages belong to — REQUIRED, because every pane that
   *  renders this chrome is looking at one (#task:2041 B3). With `workspaceId`,
   *  which is the only legitimately nullable half of the coordinate, it projects
   *  the by-number cache scope so the static export reads the SAME cell the live
   *  chip resolved — including a DM (workspaceId null) whose board is the pair
   *  conversation. */
  conversationId: string;
  /** Names this pane's selection apart from another pane's on the same
   *  conversation (the channel and its open thread). */
  selectionKey: string;
  header: ScreenshotHeader | null;
}): ReactElement {
  // The screenshot byline resolves each selected author's avatar bytes STRICTLY
  // from its own `MessageAuthorDto` (#task:1792 B2) — never the live directory.
  // The directory batch (`renderCtx.agentAvatarUrls`) stays for the Markdown
  // body's inline @-mention pills, a separate axis. Active authors already
  // resolved in the live view are session-cached by url, so this is a hit. A DM
  // header's face joins the same batch: the selection may hold only the viewer's
  // own rows, so the counterpart is not always among the authors.
  const counterpart = header?.counterpart ?? null;
  const avatarSources = useMemo(() => {
    const authors = collectAuthorAvatarSources(surface.previewSnapshots.map((m) => m.author));
    if (!counterpart) {
      return authors;
    }
    const face = collectCounterpartAvatarSources([{ counterpart }]);
    return {
      agents: [...authors.agents, ...face.agents],
      users: [...authors.users, ...face.users],
    };
  }, [surface.previewSnapshots, counterpart]);
  const authorAgentAvatarUrls = useAgentAvatarObjectUrls(avatarSources.agents);
  const authorUserAvatarUrls = useUserAvatarObjectUrls(avatarSources.users);
  const nativeBar = useNativeSelectionBar({
    key: selectionKey,
    active: surface.selection.active,
    count: surface.selection.count,
    busy: surface.imagePreviewOpen,
    onCancel: surface.selection.exit,
    onCopy: () => void surface.handleCopyMarkdownTranscript(),
    onSave: surface.handleSaveImage,
  });

  return (
    <>
      {surface.selection.active && !nativeBar ? (
        <SelectionToolbar
          count={surface.selection.count}
          onCancel={surface.selection.exit}
          onCopyMarkdown={surface.handleCopyMarkdownTranscript}
          onSaveImage={surface.handleSaveImage}
        />
      ) : null}
      <ImagePreviewModal
        open={surface.imagePreviewOpen}
        onOpenChange={surface.setImagePreviewOpen}
        onExportComplete={surface.selection.exit}
        messages={surface.previewSnapshots}
        workspaceId={workspaceId}
        conversationId={conversationId}
        header={header}
        agentsByName={renderCtx.agentsByName}
        humansByName={renderCtx.humansByName}
        channelsByName={renderCtx.channelsByName}
        conversationRefsById={renderCtx.conversationRefsById}
        agentAvatarUrls={renderCtx.agentAvatarUrls}
        authorAgentAvatarUrls={authorAgentAvatarUrls}
        authorUserAvatarUrls={authorUserAvatarUrls}
        viewer={renderCtx.viewer}
        viewerAvatarObjectUrl={renderCtx.viewerAvatarObjectUrl}
      />
    </>
  );
}
