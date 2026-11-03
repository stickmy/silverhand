import { MobileNavigationSlot } from "@/components/layout/mobile-header-slot";
import { useNativeBar } from "@/components/layout/native-page-bar";
import { DirectDraftSkeleton } from "./direct-draft-skeleton";
import { getCurrentUserId } from "@/features/session/current-user";
import { AnimatePresence } from "motion/react";
import {
  useEffect,
  useRef,
  useState,
  type ClipboardEvent,
  type DragEvent,
  type FormEvent,
  type KeyboardEvent,
} from "react";
import { formatAttachmentLabel, attachmentLabelKind } from "@silverhand/shared";
import { Button } from "@/components/ui/button";
import { AttachFileIcon } from "@/icons";
import {
  COMPOSER_TOOL_CLASS,
  effectiveContentType,
  resizeTextarea,
} from "@/features/messages/components/composer";
import { Lightbox } from "@/features/messages/components/lightbox";
import {
  AttachmentChip,
  PendingAttachmentMotion,
  PendingAttachmentTray,
} from "@/features/messages/components/pending-attachment-tray";
import type { CachedPendingAttachment } from "@/features/messages/composer-draft-cache";
import { MAX_ATTACHMENTS } from "@/features/messages/model/oversize-paste";
import {
  useNativeComposer,
  useNativeComposerPlatform,
} from "@/features/messages/hooks/use-native-composer";
import { EmptyConversation } from "@/features/conversation-shell/components/channel-view/empty-conversation";
import {
  ConversationHeaderIdentity,
  useHeaderIdentityIcon,
} from "@/features/conversation-shell/components/channel-view/conversation-header-identity";
import { focusAgentSideSection } from "@/features/agents/components/agent-view";
import { useNav } from "@/router";
import { paneChrome } from "@/components/layout/pane-chrome";
import { ApiError } from "@/lib/api";
import type { DirectMessageTarget } from "./target";
import { useDraftCounterpart } from "./use-draft-counterpart";

import { decideDraftHandoff, useDirectPairs, useInitiateDirectMessage } from "./hooks";
import { t } from "@/lib/i18n";

interface DirectPairDraftPaneProps {
  target: DirectMessageTarget;
  onOpenPair: (conversationId: string) => void;
}

export function DirectPairDraftPane({ target, onOpenPair }: DirectPairDraftPaneProps): JSX.Element {
  const targetQuery = useDraftCounterpart(target);
  const counterpart = targetQuery.counterpart;
  const initiate = useInitiateDirectMessage(target);
  const [draft, setDraft] = useState("");
  const draftFiles = useDraftFiles();
  const files = draftFiles.items;
  const [previewing, setPreviewing] = useState<{ url: string; alt: string; file?: File } | null>(
    null,
  );
  const [dragOver, setDragOver] = useState(false);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const attemptRef = useRef<{ id: string; userId: string; target: DirectMessageTarget }>();
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const pairs = useDirectPairs();
  const existingPair = pairs.find(
    (pair) => pair.counterpart.kind === target.kind && pair.counterpart.id === target.id,
  );

  // The line can come into existence without this pane's help — the participant
  // messaged the viewer first while the draft sat open, or our own first send
  // MINTED the pair and then failed to persist the message. Whether that means
  // "hand over" is one pure rule ({@link decideDraftHandoff}), not conditions
  // spread through this effect: the pane holds two things that exist nowhere else
  // (the unsent text and the unread failure), and an automatic handoff while
  // either is live discards it.
  const trimmed = draft.trim();
  const handoff = decideDraftHandoff({
    existingPairId: existingPair?.conversation_id ?? null,
    draftDirty: trimmed.length > 0 || files.length > 0,
    sendPending: initiate.isPending,
    sendFailed: initiate.isError,
  });
  const handoffTo = handoff.kind === "handoff" ? handoff.conversationId : null;
  useEffect(() => {
    if (handoffTo === null) return;
    onOpenPair(handoffTo);
  }, [handoffTo, onOpenPair]);

  useEffect(() => {
    resizeTextarea(textareaRef.current);
  }, [draft]);

  // While the send is in flight the draft is frozen: the request already holds
  // its text and files, so an edit now would be lost or sent unseen.
  const sending = initiate.isPending;
  const sendDisabled = (trimmed.length === 0 && files.length === 0) || sending;
  // Same label rule the pair's own header will use once it exists
  // (`presentCounterpart`): the display name when the participant set one, the handle
  // otherwise — so the title does not change under the user on convergence.
  const title =
    counterpart?.status === "active" ? counterpart.display_name.trim() || counterpart.name : "";
  // The same title capsule the pair's own bar shows: the face and name, opening
  // the agent's Activity or the human's profile.
  const nav = useNav();
  const titleIcon = useHeaderIdentityIcon("counterpart", counterpart ?? null);
  const active = counterpart?.status === "active" ? counterpart : null;
  const native = useNativeBar(
    counterpart
      ? {
          title,
          titleAction: active
            ? active.kind === "agent"
              ? t("directPairs.opensActivity")
              : t("directPairs.opensProfile")
            : null,
          titleIcon: active ? titleIcon : undefined,
          actions: [],
          tabs: null,
          addTabs: null,
        }
      : null,
    {
      title: () => {
        if (active?.kind === "agent") {
          focusAgentSideSection(active.id, "activity");
          nav.openAgentPanel(active.id);
        } else if (active) {
          nav.openHumanPanel(active.id);
        }
      },
    },
  );

  const submit = (text: string = draft): void => {
    const content = text.trim();
    if (sending || (content.length === 0 && files.length === 0)) return;
    const userId = getCurrentUserId();
    if (!userId) return;
    if (
      !attemptRef.current ||
      attemptRef.current.userId !== userId ||
      attemptRef.current.target.kind !== target.kind ||
      attemptRef.current.target.id !== target.id
    ) {
      attemptRef.current = { id: crypto.randomUUID(), userId, target };
    }
    void initiate
      .mutateAsync({
        content,
        client_message_id: attemptRef.current.id,
        attachments: files.map((item) => ({ file: item.file!, label: item.label! })),
      })
      .then((ack) => {
        if (getCurrentUserId() !== userId) return;
        attemptRef.current = undefined;
        setDraft("");
        draftFiles.clear();
        onOpenPair(ack.message.conversation_id);
      })
      .catch(() => {
        // The draft stays exactly as typed and the reason renders below the
        // composer from `initiate.error`. Both survive a pair appearing
        // underneath us mid-failure, because the handoff rule stays put while
        // either is live.
      });
  };

  const nativeComposer = useNativeComposerPlatform();
  const [accessoryHeight, setAccessoryHeight] = useState(0);
  const accessoryRef = useRef<HTMLDivElement>(null);
  const nativeHostRef = useRef<HTMLDivElement>(null);
  useNativeComposer({
    host: nativeHostRef,
    shown: nativeComposer && counterpart !== null,
    draftKey: `direct-draft:${target.kind}:${target.id}`,
    conversationId: null,
    placeholder: t("directPairs.message"),
    enabled: !sending,
    canSend: !sendDisabled,
    offersMentions: false,
    offersChannels: false,
    accessoryHeight,
    picker: null,
    agentAvatarUrls: NO_AVATARS,
    humanAvatarUrls: NO_AVATARS,
    activityAgentIds: null,
    draft,
    caret: draft.length,
    onChange: (text) => {
      attemptRef.current = undefined;
      setDraft(text);
    },
    onSend: (text) => {
      setDraft(text);
      submit(text);
    },
    onPick: () => {},
    onTrigger: () => {},
    onFiles: (list) => addFiles(list),
  });

  useEffect(() => {
    const node = accessoryRef.current;
    if (!node) {
      return;
    }
    const observer = new ResizeObserver(() => setAccessoryHeight(Math.ceil(node.offsetHeight)));
    observer.observe(node);
    return () => observer.disconnect();
  }, [nativeComposer, counterpart]);

  const onSubmit = (e: FormEvent): void => {
    e.preventDefault();
    submit();
  };

  // Any change to the files is a different message, so a retry after it
  // must not reuse the previous attempt's identity.
  const addFiles = (list: FileList | File[]): void => {
    if (sending || list.length === 0) return;
    attemptRef.current = undefined;
    draftFiles.add(Array.from(list));
  };

  const removeFile = (key: string): void => {
    if (sending) return;
    attemptRef.current = undefined;
    draftFiles.remove(key);
  };

  const insertLabel = (label: string): void => {
    if (sending) return;
    const el = textareaRef.current;
    const pos = el?.selectionStart ?? draft.length;
    const before = draft.slice(0, pos);
    const insertion = `${before.length > 0 && !/\s$/.test(before) ? " " : ""}${label} `;
    const caret = before.length + insertion.length;
    attemptRef.current = undefined;
    setDraft(before + insertion + draft.slice(pos));
    requestAnimationFrame(() => {
      el?.focus();
      el?.setSelectionRange(caret, caret);
    });
  };

  const onPaste = (e: ClipboardEvent<HTMLTextAreaElement>): void => {
    const pasted = e.clipboardData?.files;
    if (!pasted || pasted.length === 0) return;
    e.preventDefault();
    addFiles(pasted);
  };

  const onDrop = (e: DragEvent<HTMLFormElement>): void => {
    e.preventDefault();
    setDragOver(false);
    if (e.dataTransfer.files.length > 0) addFiles(e.dataTransfer.files);
  };

  const onDragOver = (e: DragEvent<HTMLFormElement>): void => {
    if (sending || !Array.from(e.dataTransfer.types).includes("Files")) return;
    e.preventDefault();
    setDragOver(true);
  };

  const onDragLeave = (e: DragEvent<HTMLFormElement>): void => {
    if (e.currentTarget === e.target) setDragOver(false);
  };

  // IME-safe Enter, the same rule the real composer follows: a candidate-
  // confirming Enter must never send. Tracked via composition events because
  // `isComposing` alone is unreliable on some Chinese/Japanese IMEs.
  const [composing, setComposing] = useState(false);
  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>): void => {
    if (e.key !== "Enter") return;
    if (e.shiftKey) return;
    if (composing || e.nativeEvent.isComposing) return;
    e.preventDefault();
    submit();
  };

  const sendError = initiate.isError ? (
    <div className="mb-2 text-xs text-error" role="alert">
      {friendlyInitiateError(initiate.error, title, files.length > 0)}
    </div>
  ) : null;
  const tray = (
    <AnimatePresence initial={false}>
      {files.length > 0 ? (
        <PendingAttachmentTray key="attachments">
          <AnimatePresence initial={false} mode="popLayout" propagate>
            {files.map((item) => (
              <PendingAttachmentMotion key={item.key}>
                <AttachmentChip
                  item={item}
                  onRemove={() => removeFile(item.key)}
                  removeDisabled={sending}
                  onInsertLabel={sending ? undefined : () => insertLabel(item.label!)}
                  onPreview={
                    item.previewUrl
                      ? () =>
                          setPreviewing({
                            url: item.previewUrl!,
                            alt: item.filename,
                            file: item.file,
                          })
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
  const lightbox = (
    <Lightbox
      open={previewing !== null}
      onOpenChange={(open) => {
        if (!open) setPreviewing(null);
      }}
      src={previewing?.url ?? ""}
      alt={previewing?.alt ?? ""}
      file={previewing?.file}
    />
  );

  if (targetQuery.isLoading) {
    return (
      <main data-panel="chat" className="flex h-full min-w-0 flex-1 flex-col" style={paneChrome}>
        <DirectDraftSkeleton />
      </main>
    );
  }
  if (!counterpart) {
    // The route names a participant the viewer cannot read — the same answered
    // absence the pair arm renders, in the same words.
    return (
      <main
        data-panel="chat"
        className="flex h-full min-w-0 flex-1 items-center justify-center text-sm text-text-muted"
        style={paneChrome}
      >
        {t("directPairs.thisConversationIsntAvailable")}
      </main>
    );
  }

  return (
    <main
      data-panel="chat"
      className="relative flex h-full min-w-0 flex-1 flex-col overflow-hidden"
      style={paneChrome}
    >
      {native.active ? (
        native.clearance
      ) : (
        <header
          data-conversation-header
          className="flex h-12 min-w-0 shrink-0 items-center gap-2 border-b border-border-subtle px-4"
        >
          <MobileNavigationSlot />
          <ConversationHeaderIdentity
            identity="counterpart"
            title={title}
            counterpart={counterpart}
          />
        </header>
      )}

      <div className="relative min-h-0 flex-1">
        <EmptyConversation />
      </div>

      {nativeComposer ? (
        // The native composer floats over the page; this holds the controls
        // that float above it.
        <div ref={nativeHostRef} data-native-composer>
          <div ref={accessoryRef} data-native-composer-accessory>
            {sendError}
            {tray}
          </div>
        </div>
      ) : (
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
          {sendError}
          {tray}
          <div
            data-composer-shell
            className="flex flex-col overflow-hidden rounded-(--control-radius) border border-border-subtle bg-bg-floating focus-within:border-border-focus"
          >
            <div data-composer-editor>
              <textarea
                ref={textareaRef}
                autoFocus
                value={draft}
                data-composer-input
                disabled={sending}
                onChange={(e) => {
                  attemptRef.current = undefined;
                  setDraft(e.target.value);
                }}
                onKeyDown={onKeyDown}
                onPaste={onPaste}
                onCompositionStart={() => setComposing(true)}
                onCompositionEnd={() => setComposing(false)}
                aria-label={t("directPairs.messageNamed", { name: title })}
                placeholder={t("directPairs.message")}
                className="block max-h-48 min-h-15 w-full resize-none bg-transparent px-3 py-2 text-sm text-text-primary placeholder:text-text-muted focus:outline-none disabled:cursor-not-allowed disabled:opacity-60"
              />
            </div>
            <input
              ref={fileInputRef}
              type="file"
              multiple
              className="hidden"
              onChange={(e) => {
                if (e.target.files) addFiles(e.target.files);
                e.target.value = "";
              }}
            />
            <div data-composer-toolbar className="flex items-center justify-between px-2 py-1.5">
              <button
                type="button"
                onClick={() => fileInputRef.current?.click()}
                disabled={sending || files.length >= MAX_ATTACHMENTS}
                title={t("directPairs.attachAFile")}
                aria-label={t("directPairs.attachAFile")}
                className={COMPOSER_TOOL_CLASS}
              >
                <AttachFileIcon className="size-4" />
              </button>
              <Button
                type="submit"
                variant="default"
                size="sm"
                disabled={sendDisabled}
                title={t("directPairs.sendEnter")}
                data-composer-action="send"
                data-sticker
                data-sticker-size="md"
                className="h-7"
              >
                {sending ? t("directPairs.sending") : t("directPairs.send")}
              </Button>
            </div>
          </div>
        </form>
      )}
      {lightbox}
    </main>
  );
}

const NO_AVATARS = new Map<string, string | null>();

type DraftFile = CachedPendingAttachment & { file: File; label: string };

/**
 * The files of an unsent first message. They stay on this device until Send,
 * because there is no conversation to upload them into before then. Labels are
 * assigned once and never renumbered, the same rule the conversation composer
 * keeps, so a reference typed into the text keeps pointing at its file.
 */
function useDraftFiles() {
  const [items, setItems] = useState<DraftFile[]>([]);
  const sequence = useRef({ image: 0, file: 0 });
  const itemsRef = useRef(items);
  itemsRef.current = items;

  useEffect(
    () => () => {
      for (const item of itemsRef.current) {
        if (item.previewUrl) URL.revokeObjectURL(item.previewUrl);
      }
    },
    [],
  );

  const add = (list: File[]): void => {
    const room = MAX_ATTACHMENTS - itemsRef.current.length;
    if (room <= 0) return;
    const next = list.slice(0, room).map((file): DraftFile => {
      const contentType = effectiveContentType(file);
      const kind = attachmentLabelKind(contentType);
      const ordinal = kind === "Image" ? ++sequence.current.image : ++sequence.current.file;
      return {
        key: `draft-${crypto.randomUUID()}`,
        filename: file.name,
        size: file.size,
        contentType,
        state: "done",
        file,
        previewUrl: kind === "Image" ? URL.createObjectURL(file) : undefined,
        label: formatAttachmentLabel(kind, ordinal),
      };
    });
    itemsRef.current = [...itemsRef.current, ...next];
    setItems(itemsRef.current);
  };

  const remove = (key: string): void => {
    const gone = itemsRef.current.find((item) => item.key === key);
    if (gone?.previewUrl) URL.revokeObjectURL(gone.previewUrl);
    itemsRef.current = itemsRef.current.filter((item) => item.key !== key);
    setItems(itemsRef.current);
  };

  const clear = (): void => {
    for (const item of itemsRef.current) {
      if (item.previewUrl) URL.revokeObjectURL(item.previewUrl);
    }
    itemsRef.current = [];
    sequence.current = { image: 0, file: 0 };
    setItems([]);
  };

  return { items, add, remove, clear };
}

/** Surface the server's admission result; a visible profile is not authorization. */
function friendlyInitiateError(error: Error, target: string, hasFiles: boolean): string {
  if (error instanceof ApiError) {
    if (error.kind === "backend_unreachable") {
      return t("directPairs.cantReachTheServer");
    }
    if (error.status === 403) return t("directPairs.cantStart", { name: target });
    if (error.status === 404) return t("directPairs.noLongerAvailable", { name: target });
    if (error.status === 413) {
      return hasFiles ? t("directPairs.attachmentsMustBe10") : t("directPairs.thatMessageIsToo");
    }
    if (error.status === 400 && hasFiles) return t("directPairs.oneOfTheAttachments");
    if (error.status === 400) return t("directPairs.thatMessageCantBe");
  }
  return t("directPairs.couldNotSendPlease");
}
