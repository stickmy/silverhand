import { AnimatePresence } from "motion/react";
import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type ClipboardEvent,
  type DragEvent,
  type FormEvent,
  type KeyboardEvent,
} from "react";
import { AttachFileIcon, HashIcon, MentionIcon } from "@/icons";
import { Button } from "@/components/ui/button";
import type { ComposerDirectory } from "@/features/conversations/pane-mode";
import { pendingAttachmentsBlockSend } from "@/features/messages/composer-draft-cache";
import { onQuoteToComposer, spliceQuoteIntoDraft } from "@/features/messages/quote-to-composer";
import { useComposerDirectory } from "../hooks/use-composer-directory";
import { useComposerDraft } from "../hooks/use-composer-draft";
import { useNativeComposer, useNativeComposerPlatform } from "../hooks/use-native-composer";
import {
  detectTrigger,
  formatPickInsertion,
  selectPickerView,
  type PickerItem,
} from "../model/composer-picker";
import { MAX_ATTACHMENTS, shouldInterceptOversizedPaste } from "../model/oversize-paste";
import { ComposerActivityHeader } from "./composer-activity-header";
import { PickerMenu } from "./composer-picker-menu";
import { pickerViewOf } from "../model/composer-picker-rows";
import { Lightbox } from "./lightbox";
import {
  AttachmentChip,
  PendingAttachmentMotion,
  PendingAttachmentTray,
} from "./pending-attachment-tray";
import { t } from "@/lib/i18n";

export { effectiveContentType } from "../hooks/use-composer-draft";

interface ComposerProps {
  conversationId: string;
  /** Where `@` and `#` candidates come from, projected once by the pane owner. */
  directory: ComposerDirectory;
  /** Replies into this root's reply cluster, with a draft of its own. */
  threadRootId?: string;
  disabled: boolean;
  archived?: boolean;
  /** The conversation can never accept another message (a direct pair whose
   *  counterpart is gone). */
  readOnly?: boolean;
  /** Limits the activity bubble to these agents; null shows all. */
  activityAgentIds?: readonly string[] | null;
  /** Another view covers the composer (a view tab in front of the chat). */
  covered?: boolean;
}

const MAX_TEXTAREA_HEIGHT = 192; // Tailwind max-h-48.

/** The toolbar's icon buttons: quiet, ground only on hover. The composer shell
 *  is already the frame. */
export const COMPOSER_TOOL_CLASS =
  "inline-flex size-7 items-center justify-center rounded-md text-text-muted transition-colors hover:bg-bg-elevated hover:text-text-primary disabled:cursor-not-allowed disabled:opacity-50";

/**
 * Message composer with `@` mention and `#` channel pickers.
 *
 * `@` or `#` opens the picker unless the character before it ends another
 * token (an email, a path); CJK prose needs no space. While the picker has
 * matches, Enter and Tab pick instead of sending.
 */
export function Composer({
  conversationId,
  directory,
  threadRootId,
  disabled: disabledProp,
  archived = false,
  readOnly = false,
  activityAgentIds,
  covered = false,
}: ComposerProps): JSX.Element {
  // Every send and attach path gates on one flag; only the placeholder tells
  // the reasons apart.
  const disabled = disabledProp || archived || readOnly;
  const sources = useComposerDirectory(directory, conversationId);
  const composer = useComposerDraft({
    conversationId,
    threadRootId,
    disabled,
    mentionSource: sources.mentionSource,
    refetchMentionResolver: sources.refetchMentionResolver,
  });
  const { draftKey, draft, caret, pending } = composer;

  // `isComposing` is unreliable on some CJK IMEs: the Enter that confirms a
  // candidate can arrive with it false, so composition is tracked explicitly.
  const [composing, setComposing] = useState(false);
  const [dragOver, setDragOver] = useState(false);
  const [active, setActive] = useState(0);
  // Escape hides the picker for the trigger at this position.
  const [dismissedPos, setDismissedPos] = useState<number | null>(null);
  const [previewing, setPreviewing] = useState<{ url: string; alt: string } | null>(null);
  const ref = useRef<HTMLTextAreaElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);

  const offersMentions = directory.kind !== "withheld";
  const offersChannels = sources.channelSource.offersChannels;
  const trigger = useMemo(
    () => (composing ? null : detectTrigger(draft, caret)),
    [draft, caret, composing],
  );
  const { items, hiddenCount, channelStatus } = useMemo(
    () => selectPickerView(trigger, sources.mentionSource, sources.channelSource),
    [trigger, sources.mentionSource, sources.channelSource],
  );
  const peopleSections = directory.kind !== "withheld" && trigger?.kind === "@";
  // Cross-workspace channel rows name their workspace.
  const showChannelWorkspace = directory.kind === "viewer" && trigger?.kind === "#";
  // `#` opens on zero rows while its source loads, so loading does not read as
  // "no channels"; `@` opens only on rows.
  const menuOpen = !!trigger && (items.length > 0 || channelStatus !== "complete");
  const dismissed = trigger != null && dismissedPos === trigger.pos;
  const pickerShown = menuOpen && !dismissed;
  const pickerView = useMemo(
    () => pickerViewOf({ items, hiddenCount, channelStatus, peopleSections, showChannelWorkspace }),
    [items, hiddenCount, channelStatus, peopleSections, showChannelWorkspace],
  );
  const sendDisabled =
    disabled ||
    composer.sending ||
    pendingAttachmentsBlockSend(pending) ||
    (!draft.trim() && pending.filter((p) => p.state === "done").length === 0);

  const native = useNativeComposerPlatform();
  const [accessoryHeight, setAccessoryHeight] = useState(0);
  const accessoryRef = useRef<HTMLDivElement>(null);
  const nativeHostRef = useRef<HTMLDivElement>(null);
  const placeholder = archived
    ? t("messages.thisConversationIsArchived")
    : readOnly
      ? "This conversation is read-only"
      : disabled
        ? t("messages.thisConversationIsClosed")
        : t("messages.message");
  const nativeComposer = useNativeComposer({
    host: nativeHostRef,
    shown: native && !covered,
    draftKey,
    conversationId,
    placeholder,
    enabled: !disabled,
    canSend: !sendDisabled,
    offersMentions,
    offersChannels,
    accessoryHeight,
    picker: pickerShown ? pickerView : null,
    agentAvatarUrls: sources.agentAvatarUrls,
    humanAvatarUrls: sources.humanAvatarUrls,
    activityAgentIds,
    draft,
    caret,
    onChange: (text, nextCaret, nextComposing) => {
      setComposing(nextComposing);
      composer.updateDraft(text, nextCaret);
    },
    onSend: (text, nextCaret) => {
      setComposing(false);
      if (text !== composer.current().text) {
        composer.updateDraft(text, nextCaret);
      }
      composer.submit();
    },
    onPick: insertPick,
    onTrigger: insertTrigger,
    onFiles: (files) => composer.addFiles(files),
  });

  useEffect(() => {
    resizeTextarea(ref.current);
  }, [draft]);

  useEffect(() => {
    const node = accessoryRef.current;
    if (!node) {
      return;
    }
    const observer = new ResizeObserver(() => setAccessoryHeight(Math.ceil(node.offsetHeight)));
    observer.observe(node);
    return () => observer.disconnect();
  }, [native, covered]);

  useEffect(() => {
    if (active >= items.length) {
      setActive(0);
    }
  }, [items.length, active]);

  useEffect(() => {
    setActive(0);
  }, [trigger?.pos, trigger?.kind]);

  useEffect(() => {
    if (!trigger) {
      if (dismissedPos !== null) {
        setDismissedPos(null);
      }
      return;
    }
    if (dismissedPos !== null && dismissedPos !== trigger.pos) {
      setDismissedPos(null);
    }
  }, [trigger, dismissedPos]);

  useEffect(() => {
    if (disabled) {
      return;
    }
    return onQuoteToComposer(draftKey, (text) => {
      const { text: current, caret: currentCaret } = composer.current();
      const pos = ref.current?.selectionStart ?? currentCaret;
      const next = spliceQuoteIntoDraft(current, pos, text);
      applyEdit(next.text, next.caret);
    });
  }, [draftKey, disabled]);

  /** Applies an edit made outside typing, then puts focus and the caret back
   *  after React writes the value (a selection set before that is lost). */
  function applyEdit(nextDraft: string, nextCaret: number): void {
    composer.updateDraft(nextDraft, nextCaret);
    if (native) {
      nativeComposer.focusText(nextDraft, nextCaret);
      return;
    }
    requestAnimationFrame(() => {
      const node = ref.current;
      if (!node) {
        return;
      }
      node.focus();
      node.setSelectionRange(nextCaret, nextCaret);
      composer.updateCaret(nextCaret);
    });
  }

  /** Inserts `text` at the caret; `pad` adds a space before it when it would
   *  otherwise join the previous word. */
  function insertAtCaret(text: string, pad: RegExp): void {
    const { text: current, caret: currentCaret } = composer.current();
    const pos = ref.current?.selectionStart ?? currentCaret;
    const before = current.slice(0, pos);
    const lead = pad.test(before) ? " " : "";
    applyEdit(before + lead + text + current.slice(pos), before.length + lead.length + text.length);
  }

  function insertPick(item: PickerItem): void {
    if (!trigger) {
      return;
    }
    const before = draft.slice(0, trigger.pos);
    const after = draft.slice(trigger.pos + 1 + trigger.query.length);
    // The trailing space ends the handle, so the next keystroke does not
    // continue it.
    const insertion = `${formatPickInsertion(item)} `;
    applyEdit(before + insertion + after, (before + insertion).length);
  }

  function insertTrigger(kind: "@" | "#"): void {
    setDismissedPos(null);
    insertAtCaret(kind, /[\w/]$/);
  }

  /** The label stays plain text: deleting it keeps the attachment. */
  function insertLabel(label: string): void {
    insertAtCaret(`${label} `, /\S$/);
  }

  function onDrop(e: DragEvent<HTMLFormElement>): void {
    e.preventDefault();
    setDragOver(false);
    if (disabled) {
      return;
    }
    if (e.dataTransfer.files && e.dataTransfer.files.length > 0) {
      composer.addFiles(e.dataTransfer.files);
    }
  }

  function onDragOver(e: DragEvent<HTMLFormElement>): void {
    if (disabled) {
      return;
    }
    if (Array.from(e.dataTransfer.types).includes("Files")) {
      e.preventDefault();
      setDragOver(true);
    }
  }

  function onDragLeave(e: DragEvent<HTMLFormElement>): void {
    // Child traversal fires dragleave too; only leaving the form counts.
    if (e.currentTarget === e.target) {
      setDragOver(false);
    }
  }

  function onPaste(e: ClipboardEvent<HTMLTextAreaElement>): void {
    if (disabled) {
      return;
    }
    // Only files are intercepted, so a plain text paste and IME stay native.
    const files = e.clipboardData?.files;
    if (files && files.length > 0) {
      e.preventDefault();
      composer.addFiles(files);
      return;
    }
    // A multi-megabyte paste becomes a text attachment instead of entering the
    // textarea. At capacity it falls through; the send path converts it then.
    const text = e.clipboardData?.getData("text/plain") ?? "";
    if (shouldInterceptOversizedPaste(text, composer.current().pendingCount)) {
      e.preventDefault();
      const stamp = new Date().toISOString().replace(/[:.]/g, "-");
      composer.addFiles([new File([text], `pasted-text-${stamp}.txt`, { type: "text/plain" })]);
    }
  }

  function onKeyDown(e: KeyboardEvent<HTMLTextAreaElement>): void {
    if (menuOpen && !dismissed) {
      if (e.key === "Escape") {
        e.preventDefault();
        if (trigger) {
          setDismissedPos(trigger.pos);
        }
        return;
      }
      if (items.length > 0) {
        if (e.key === "ArrowDown") {
          e.preventDefault();
          setActive((i) => (i + 1) % items.length);
          return;
        }
        if (e.key === "ArrowUp") {
          e.preventDefault();
          setActive((i) => (i - 1 + items.length) % items.length);
          return;
        }
        if (e.key === "Enter" || e.key === "Tab") {
          if (composing || e.nativeEvent.isComposing) {
            return;
          }
          e.preventDefault();
          const pick = items[active];
          if (pick) {
            insertPick(pick);
          }
          return;
        }
      }
    }

    if (e.key !== "Enter" || e.shiftKey) {
      return;
    }
    if (composing || e.nativeEvent.isComposing) {
      return;
    }
    e.preventDefault();
    composer.submit();
  }

  function onSubmit(e: FormEvent): void {
    e.preventDefault();
    composer.submit();
  }

  const tray = (
    <AnimatePresence initial={false} key={draftKey}>
      {pending.length > 0 ? (
        <PendingAttachmentTray key="attachments">
          <AnimatePresence initial={false} mode="popLayout" propagate>
            {pending.map((p) => (
              <PendingAttachmentMotion key={p.key}>
                <AttachmentChip
                  item={p}
                  onRemove={() => composer.removePending(p.key)}
                  onInsertLabel={
                    p.label && p.state !== "error" ? () => insertLabel(p.label!) : undefined
                  }
                  onPreview={
                    p.previewUrl
                      ? () => setPreviewing({ url: p.previewUrl!, alt: p.filename })
                      : undefined
                  }
                />
              </PendingAttachmentMotion>
            ))}
          </AnimatePresence>
        </PendingAttachmentTray>
      ) : null}
    </AnimatePresence>
  );
  const picker =
    pickerShown && !native ? (
      <PickerMenu
        view={pickerView}
        active={active}
        onPick={insertPick}
        onHover={setActive}
        agentAvatarUrls={sources.agentAvatarUrls}
        humanAvatarUrls={sources.humanAvatarUrls}
      />
    ) : null;
  const lightbox = (
    <Lightbox
      open={previewing !== null}
      onOpenChange={(open) => {
        if (!open) {
          setPreviewing(null);
        }
      }}
      src={previewing?.url ?? ""}
      alt={previewing?.alt ?? ""}
    />
  );

  if (native) {
    // The native composer floats over the page; this holds the room the page
    // gives it and the controls that float above it.
    return (
      <div ref={nativeHostRef} data-native-composer aria-hidden={covered || undefined}>
        {covered ? null : (
          <div ref={accessoryRef} data-native-composer-accessory>
            {tray}
          </div>
        )}
        {lightbox}
      </div>
    );
  }

  return (
    <form
      data-composer
      onSubmit={onSubmit}
      onDrop={onDrop}
      onDragOver={onDragOver}
      onDragLeave={onDragLeave}
      className={
        "shrink-0 border-t border-border-subtle bg-(--pane-bg,var(--bg-base)) px-3 py-3 " +
        (dragOver ? "[outline:2px_solid_var(--border-focus)] -outline-offset-2" : "")
      }
    >
      {tray}
      <div data-composer-frame className="relative">
        {covered ? null : (
          // Sits on the shell's top edge but out of flow, so a run starting or
          // ending never resizes the message list. Hidden under a covering tab.
          <ComposerActivityHeader
            conversationId={conversationId}
            restrictToAgentIds={activityAgentIds}
          />
        )}
        {picker}
        <div
          data-composer-shell
          className="flex flex-col overflow-hidden rounded-(--control-radius) border border-border-subtle bg-bg-floating focus-within:border-border-focus"
        >
          <div data-composer-editor>
            <textarea
              ref={ref}
              value={draft}
              data-composer-input
              onChange={(e) => {
                composer.updateDraft(
                  e.target.value,
                  e.target.selectionStart ?? e.target.value.length,
                );
              }}
              onKeyUp={(e) => composer.updateCaret(e.currentTarget.selectionStart ?? 0)}
              onClick={(e) => composer.updateCaret(e.currentTarget.selectionStart ?? 0)}
              onSelect={(e) => composer.updateCaret(e.currentTarget.selectionStart ?? 0)}
              onKeyDown={onKeyDown}
              onPaste={onPaste}
              onCompositionStart={() => setComposing(true)}
              onCompositionEnd={() => setComposing(false)}
              disabled={disabled}
              placeholder={placeholder}
              className="block max-h-48 min-h-15 w-full resize-none bg-transparent px-3 py-2 text-sm text-text-primary placeholder:text-text-muted focus:outline-none disabled:cursor-not-allowed disabled:opacity-60"
            />
          </div>
          <input
            ref={fileInputRef}
            type="file"
            multiple
            className="hidden"
            onChange={(e) => {
              if (e.target.files) {
                composer.addFiles(e.target.files);
              }
              // Reset, so choosing the same file again still fires.
              e.target.value = "";
            }}
          />
          <div data-composer-toolbar className="flex items-center justify-between px-2 py-1.5">
            {/* `@` and `#` are absent, not disabled, where the arm offers no
                candidates: a picker that opens on nothing is worse than none. */}
            <div className="flex items-center gap-0.5">
              <button
                type="button"
                onClick={() => fileInputRef.current?.click()}
                disabled={disabled || pending.length >= MAX_ATTACHMENTS}
                title={t("messages.attachAFile")}
                className={COMPOSER_TOOL_CLASS}
              >
                <AttachFileIcon className="size-4" />
              </button>
              {offersMentions ? (
                <button
                  type="button"
                  onClick={() => insertTrigger("@")}
                  disabled={disabled}
                  title={t("messages.mentionAMember")}
                  aria-label={t("messages.mentionAMember")}
                  data-composer-tool="mention"
                  className={COMPOSER_TOOL_CLASS}
                >
                  <MentionIcon className="size-4" />
                </button>
              ) : null}
              {offersChannels ? (
                <button
                  type="button"
                  onClick={() => insertTrigger("#")}
                  disabled={disabled}
                  title={t("messages.referenceAChannel")}
                  aria-label={t("messages.referenceAChannel")}
                  data-composer-tool="channel"
                  className={COMPOSER_TOOL_CLASS}
                >
                  <HashIcon className="size-4" />
                </button>
              ) : null}
            </div>
            <Button
              type="submit"
              variant="default"
              size="sm"
              disabled={sendDisabled}
              title={t("messages.sendEnter")}
              data-composer-action="send"
              data-sticker
              data-sticker-size="md"
              className="h-7"
            >
              {t("messages.send")}
            </Button>
          </div>
        </div>
      </div>
      {lightbox}
    </form>
  );
}

export function resizeTextarea(el: HTMLTextAreaElement | null): void {
  if (!el) {
    return;
  }
  el.style.height = "auto";
  const nextHeight = Math.min(el.scrollHeight, MAX_TEXTAREA_HEIGHT);
  el.style.height = `${nextHeight}px`;
  el.style.overflowY = el.scrollHeight > MAX_TEXTAREA_HEIGHT ? "auto" : "hidden";
}
