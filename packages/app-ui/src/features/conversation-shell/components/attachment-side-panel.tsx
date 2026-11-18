import type { AttachmentSnapshot } from "@silverhand/shared";
import { paneChrome } from "@/components/layout/pane-chrome";
import { SidePanelHeader, SidePanelHeaderTitle } from "@/components/layout/side-panel-header";
import { iconMarkup } from "@/components/layout/native-page-bar";
import { useCompactLayout } from "@/navigation/layout";
import { getPlatform } from "@/platform";
import { DocumentIcon, DownloadIcon, OpenExternalIcon, ExpandIcon } from "@/icons";
import { t } from "@/lib/i18n";
import { openAttachmentExternally } from "@/features/messages/model/open-attachment";
import { saveAttachmentToDisk } from "@/features/messages/model/save-attachment";
import {
  AttachmentDownloadGlyph,
  AttachmentTaskGlyph,
} from "@/features/messages/components/attachment-task-glyph";
import { AttachmentPreviewBody, useAttachmentPreview } from "./channel-view/channel-attachment-tab";

const ACTION_CLASS =
  "flex size-7 shrink-0 items-center justify-center rounded-(--control-radius) text-text-secondary transition-colors hover:bg-bg-hover hover:text-text-primary focus-outline";

/**
 * A md/html attachment previewed in the right pane, beside the conversation.
 * The route carries only the id, so the name and type come from the fetched
 * bytes. Expand hands the preview to the main pane's tab and is withheld on
 * compact, where this panel is already a full page.
 */
export function AttachmentSidePanel({
  attachmentId,
  onClose,
  onBack,
  onExpand,
}: {
  attachmentId: string;
  onClose: () => void;
  /** Returns to the list this preview was opened from. */
  onBack?: () => void;
  onExpand?: (attachment: AttachmentSnapshot) => void;
}): JSX.Element {
  const compact = useCompactLayout();
  const { state, htmlZoom, onPinch } = useAttachmentPreview(attachmentId, "", "");

  const ready = state.status === "ready" ? state : null;
  const filename = ready?.filename ?? "";
  const snapshot: AttachmentSnapshot | null = ready
    ? { id: attachmentId, filename, size: ready.size, content_type: ready.contentType }
    : null;
  const expand = !compact && onExpand && snapshot ? () => onExpand(snapshot) : null;
  const canOpenExternal = Boolean(getPlatform().files.openAttachmentExternal);
  const openExternal = () => void openAttachmentExternally({ id: attachmentId, filename });
  const download = () => void saveAttachmentToDisk({ id: attachmentId, filename });

  return (
    <aside
      data-panel="side"
      data-pane="attachment"
      className="flex h-full min-h-0 w-full flex-col overflow-hidden"
      style={paneChrome}
    >
      <SidePanelHeader
        onClose={onClose}
        onBack={onBack}
        nativeBar={{
          title: filename,
          titleAction: null,
          actions: ready
            ? [
                [
                  ...(canOpenExternal
                    ? [
                        {
                          id: "open",
                          label: t("conversationShell.openInDefaultApp"),
                          icon: iconMarkup(OpenExternalIcon),
                        },
                      ]
                    : []),
                  {
                    id: "download",
                    label: t("conversationShell.download"),
                    icon: iconMarkup(DownloadIcon),
                  },
                ],
              ]
            : [],
          tabs: null,
          addTabs: null,
        }}
        nativeHandlers={{
          action: ({ id }) => {
            if (id === "open") {
              openExternal();
            } else if (id === "download") {
              download();
            }
          },
        }}
        actions={
          ready ? (
            <>
              {expand ? (
                <button
                  type="button"
                  onClick={expand}
                  data-sticker
                  data-sticker-size="md"
                  data-sticker-tone="neutral"
                  className={ACTION_CLASS}
                  aria-label={t("conversationShell.openInMainPane")}
                  title={t("conversationShell.openInMainPane")}
                >
                  <ExpandIcon className="size-3.5" />
                </button>
              ) : null}
              {canOpenExternal ? (
                <button
                  type="button"
                  onClick={openExternal}
                  data-sticker
                  data-sticker-size="md"
                  data-sticker-tone="neutral"
                  className={ACTION_CLASS}
                  aria-label={t("conversationShell.openInDefaultApp")}
                  title={t("conversationShell.openInDefaultApp")}
                >
                  <AttachmentTaskGlyph task="open" attachmentId={attachmentId}>
                    <OpenExternalIcon className="size-3.5" />
                  </AttachmentTaskGlyph>
                </button>
              ) : null}
              <button
                type="button"
                onClick={download}
                data-sticker
                data-sticker-size="md"
                data-sticker-tone="neutral"
                className={ACTION_CLASS}
                aria-label={t("conversationShell.download")}
                title={t("conversationShell.download")}
              >
                <AttachmentDownloadGlyph attachmentId={attachmentId} />
              </button>
            </>
          ) : undefined
        }
      >
        <SidePanelHeaderTitle
          icon={<DocumentIcon className="size-3.5 shrink-0 text-text-muted" />}
          title={filename}
        />
      </SidePanelHeader>
      <AttachmentPreviewBody
        state={state}
        filename={filename}
        fallbackContentType=""
        htmlZoom={htmlZoom}
        onPinch={onPinch}
      />
    </aside>
  );
}
