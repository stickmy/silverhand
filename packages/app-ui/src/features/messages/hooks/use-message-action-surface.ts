import { useCallback, useEffect, useMemo, useState } from "react";
import { toast } from "@/lib/toast";
import type { MessageDto } from "@silverhand/shared";
import {
  useMessageSelection,
  type MessageSelectionHook,
} from "@/features/messages/hooks/use-message-selection";
import { buildMarkdownTranscript } from "@/features/messages/model/markdown-transcript";
import { type CachedMessage, isCommittedMessage } from "@/features/channels/pending-message";
import { useSavedMessageIds, useToggleSave } from "@/features/messages/saved";
import { t } from "@/lib/i18n";

/** Per-row action props spread onto `MessageItem` / `ChannelMessageItem`.
 *  A single bundle so neither call site can silently drop one — that
 *  hand-assembly is exactly the recurring prop drift (#task:1052 sigils,
 *  #task:1065 save, #task:1068 copy/share). */
export interface MessageRowActionProps {
  saved: boolean;
  /** Save/unsave is a member mutation. Absent (undefined) when the
   *  viewer may not mutate — a non-member browsing a public channel — which
   *  hides the Save affordance (MessageItem guards on `onSave`). Copy / share
   *  (read-side export) stay available regardless. */
  onSave?: (msg: CachedMessage) => void;
  selectionMode: boolean;
  selected: boolean;
  onToggleSelected: (msg: CachedMessage) => void;
  onCopyMarkdown: (msg: CachedMessage) => Promise<void>;
  onShare: (msg: CachedMessage) => void;
}

export interface MessageActionSurface {
  selection: MessageSelectionHook;
  imagePreviewOpen: boolean;
  setImagePreviewOpen: (open: boolean) => void;
  previewSnapshots: MessageDto[];
  /** Multi-select → copy a sender-headed transcript to the clipboard. */
  handleCopyMarkdownTranscript: () => Promise<void>;
  /** Multi-select → open the image-export preview. */
  handleSaveImage: () => void;
  /** Build the per-row action prop bundle for one message. */
  rowActionProps: (msg: CachedMessage) => MessageRowActionProps;
}

/**
 * Shared message-row ACTION surface for the channel pane and the thread
 * reply-cluster pane: save / copy-markdown / share → multi-select →
 * copy-transcript / export-image. Owns the selection state machine, the
 * clipboard + export handlers, the ESC-to-exit binding, and the per-row
 * action prop bundle. Render the toolbar + image-export modal with
 * `<MessageActionChrome>`.
 *
 * Deliberately NOT in scope (the #task:1067 boundary): the list/window
 * strategy. `ChannelView` keeps `MessageScroller` + a main-stream window reset;
 * `ThreadPanel` keeps a plain scroller + cluster window reset. Each site
 * computes its own `windowResetVersion` / `jumpToMessageId` reset signals
 * and passes them in; this hook only consumes them (via
 * `useMessageSelection`) to clear an active selection when the window the
 * user was selecting in goes away.
 */
export function useMessageActionSurface(deps: {
  channelId: string;
  /** Selection-clearing signal: bump on any disjoint window replacement. */
  windowResetVersion: number;
  jumpToMessageId: string | null;
  /** Whether save/unsave (收藏) is permitted. Defaults to `true`. `false` for a
   *  non-member browsing a public channel — save is a member
   *  mutation, so `onSave` is withheld (undefined) and the Save affordance is
   *  hidden. Copy / share (read-side export) are unaffected. */
  canSave?: boolean;
}): MessageActionSurface {
  const { channelId, windowResetVersion, jumpToMessageId, canSave = true } = deps;
  const savedIds = useSavedMessageIds();
  const { toggle: toggleSave } = useToggleSave();
  const [imagePreviewOpen, setImagePreviewOpen] = useState(false);
  const selection = useMessageSelection({
    channelId,
    windowResetVersion,
    jumpToMessageId,
  });

  // ESC exits selection mode. Bound only while selecting AND the image
  // preview modal is closed — Radix Dialog already consumes ESC to close
  // itself, and we don't want one keystroke both closing the modal and
  // dropping the selection (losing the user's queue for a retry). Bubble
  // phase + `defaultPrevented` check so a focused composer/input that
  // preventDefaults Escape (e.g. IME cancel) wins; don't steal ESC from
  // inputs/textareas, which have their own meaning for it.
  useEffect(() => {
    if (!selection.active || imagePreviewOpen) return;
    const onKey = (event: KeyboardEvent): void => {
      if (event.key !== "Escape" || event.defaultPrevented) return;
      const target = event.target as HTMLElement | null;
      const tag = target?.tagName;
      if (tag === "INPUT" || tag === "TEXTAREA" || target?.isContentEditable) {
        return;
      }
      event.preventDefault();
      selection.exit();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [selection, imagePreviewOpen]);

  // Every row action requires a committed server row: a pending optimistic
  // message has no server id / seq / permalink, so save / select / share /
  // copy are inert on it (~200ms until the ack swaps in the real row). This
  // replaces the old behavior where the same actions fired against a
  // `pending-<uuid>` id the server rejects.
  const handleSave = useCallback(
    (msg: CachedMessage) => {
      if (isCommittedMessage(msg)) toggleSave(msg.id);
    },
    [toggleSave],
  );
  // Single-message copy: clipboard receives ONLY `message.content`. The
  // multi-message transcript (with sender headers) is the separate
  // `handleCopyMarkdownTranscript` path — same clipboard, different shape.
  const handleCopyMarkdown = useCallback(async (msg: CachedMessage): Promise<void> => {
    if (!isCommittedMessage(msg)) return;
    try {
      await navigator.clipboard.writeText(msg.content);
    } catch {
      toast.error(t("common.copyFailed"));
    }
  }, []);
  // Per-row Share: enter selection mode with this message preselected.
  // `enter` is idempotent — re-entering just adds another message.
  const handleShare = useCallback(
    (msg: CachedMessage) => {
      if (isCommittedMessage(msg)) selection.enter(msg);
    },
    [selection],
  );
  const handleCopyMarkdownTranscript = useCallback(async (): Promise<void> => {
    const snapshots = selection.selectedSorted();
    if (snapshots.length === 0) return;
    const text = buildMarkdownTranscript(snapshots);
    try {
      await navigator.clipboard.writeText(text);
      selection.exit();
    } catch {
      toast.error(t("common.copyFailed"));
    }
  }, [selection]);
  const handleSaveImage = useCallback(() => {
    if (selection.count === 0) return;
    setImagePreviewOpen(true);
  }, [selection.count]);
  const previewSnapshots = useMemo(
    () => (imagePreviewOpen ? selection.selectedSorted() : []),
    [imagePreviewOpen, selection],
  );

  const handleToggleSelected = useCallback(
    (msg: CachedMessage) => {
      if (isCommittedMessage(msg)) selection.toggle(msg);
    },
    [selection],
  );

  const rowActionProps = useCallback(
    (msg: CachedMessage): MessageRowActionProps => ({
      saved: savedIds.has(msg.id),
      // Withhold the save handler entirely when the viewer may not mutate, so
      // the Save affordance is hidden (MessageItem guards on `onSave`).
      onSave: canSave ? handleSave : undefined,
      selectionMode: selection.active,
      selected: selection.isSelected(msg.id),
      onToggleSelected: handleToggleSelected,
      onCopyMarkdown: handleCopyMarkdown,
      onShare: handleShare,
    }),
    [
      savedIds,
      canSave,
      handleSave,
      selection.active,
      selection.isSelected,
      handleToggleSelected,
      handleCopyMarkdown,
      handleShare,
    ],
  );

  return {
    selection,
    imagePreviewOpen,
    setImagePreviewOpen,
    previewSnapshots,
    handleCopyMarkdownTranscript,
    handleSaveImage,
    rowActionProps,
  };
}
