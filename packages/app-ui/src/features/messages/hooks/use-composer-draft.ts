import { useEffect, useRef, useState } from "react";
import {
  rewriteShorthandReferences,
  type ShorthandResolver,
} from "@silverhand/shared/reference-markdown";
import { isGenericMimeHint, mimeFromFilename } from "@silverhand/shared";
import { createLogicalSend, type LogicalSend } from "@/features/channels/logical-send";
import { useSendMessage } from "@/features/channels/hooks";
import { getCurrentUserId } from "@/features/session/current-user";
import {
  assignAttachmentLabel,
  clearComposerDraft,
  clearComposerSendAttempt,
  dropSentAttachmentsFromDraft,
  hasComposerDraftInFlightUpload,
  patchComposerDraftText,
  pendingAttachmentsBlockSend,
  readComposerDraft,
  removeComposerDraftAttachment,
  startComposerDraftAttachmentUpload,
  subscribeComposerDraft,
  upsertComposerDraftAttachment,
  writeComposerDraft,
  type CachedPendingAttachment,
  type ComposerDraftSnapshot,
} from "@/features/messages/composer-draft-cache";
import { canResolveShorthandNow, type ComposerMentionSource } from "../model/composer-picker";
import { MAX_ATTACHMENTS } from "../model/oversize-paste";
import { t } from "@/lib/i18n";

export type PendingAttachment = CachedPendingAttachment;

/**
 * The content type the backend will store: a usable browser hint, else the
 * filename suffix, as the backend's own hint path does. Labels (`Image #1` /
 * `File #1`) follow it, so a hintless `.png` is not labelled a file and then
 * rejected at send as a kind mismatch.
 */
export function effectiveContentType(file: File): string {
  const hint = file.type;
  if (!isGenericMimeHint(hint)) {
    return hint;
  }
  return mimeFromFilename(file.name) ?? "application/octet-stream";
}

type SubmittingSnapshot = {
  snapshot: ComposerDraftSnapshot;
  editGeneration: number;
  sentAttachmentIds: string[];
};

interface ComposerDraftOptions {
  conversationId: string;
  threadRootId?: string;
  disabled: boolean;
  mentionSource: ComposerMentionSource;
  refetchMentionResolver: () => Promise<ShorthandResolver | null>;
}

export interface ComposerDraft {
  draftKey: string;
  draft: string;
  caret: number;
  pending: PendingAttachment[];
  sending: boolean;
  /** The latest draft, for edits made outside a render. */
  current: () => { text: string; caret: number; pendingCount: number };
  updateDraft: (text: string, caret: number) => void;
  updateCaret: (caret: number) => void;
  submit: () => void;
  addFiles: (files: FileList | File[]) => void;
  reservePending: (entries: { filename: string; contentType: string }[]) => string[];
  fillPending: (key: string, file: File) => void;
  failPending: (key: string) => void;
  removePending: (key: string) => void;
}

/**
 * One composer's draft and its send lifecycle.
 *
 * The draft persists per conversation (a reply cluster keeps its own) and
 * survives unmount. Sending clears the composer at once; a failed send puts
 * the author's own bytes back, unless they have started the next message, and
 * a retry reuses the same logical send so the server can deduplicate it.
 */
export function useComposerDraft({
  conversationId,
  threadRootId,
  disabled,
  mentionSource,
  refetchMentionResolver,
}: ComposerDraftOptions): ComposerDraft {
  // Network calls still target `conversationId`; only the draft is keyed apart.
  const draftKey = threadRootId ? `${conversationId}#cluster#${threadRootId}` : conversationId;
  const [initialDraft] = useState(() => readComposerDraft(draftKey));
  const [draft, setDraftState] = useState(initialDraft?.text ?? "");
  const [caret, setCaretState] = useState(initialDraft?.caret ?? 0);
  const [pending, setPendingState] = useState<PendingAttachment[]>(
    () => initialDraft?.pending ?? [],
  );
  // Async callbacks and the unmount write read these, not stale render state.
  const draftRef = useRef(draft);
  const caretRef = useRef(caret);
  const pendingRef = useRef<PendingAttachment[]>(pending);
  const retrySendRef = useRef<LogicalSend | undefined>(initialDraft?.sendAttempt);
  // Bumped by every edit, so a late failure never restores over a new message.
  const editGenerationRef = useRef(0);
  const mountedRef = useRef(true);
  const submittingSnapshotRef = useRef<SubmittingSnapshot | null>(null);
  const send = useSendMessage(conversationId, threadRootId);

  useEffect(() => {
    return () => {
      mountedRef.current = false;
    };
  }, []);

  // Resume uploads a previous mount left in flight.
  useEffect(() => {
    for (const item of pendingRef.current) {
      if (
        item.state === "uploading" &&
        item.file &&
        !hasComposerDraftInFlightUpload(draftKey, item.key)
      ) {
        void startComposerDraftAttachmentUpload({
          conversationId: draftKey,
          uploadChannelId: conversationId,
          attachmentKey: item.key,
          file: item.file,
          previewUrl: item.previewUrl,
        });
      }
    }
  }, [draftKey, conversationId]);

  useEffect(() => {
    return subscribeComposerDraft(draftKey, (event) => {
      if (event.type === "clear") {
        retrySendRef.current = undefined;
        draftRef.current = "";
        caretRef.current = 0;
        pendingRef.current = [];
        setDraftState("");
        setCaretState(0);
        setPendingState([]);
        return;
      }
      if (event.type === "attachment-remove") {
        setPendingValue((curr) => curr.filter((item) => item.key !== event.attachmentKey));
        return;
      }
      setPendingValue((curr) =>
        curr.map((item) => (item.key === event.attachmentKey ? { ...item, ...event.patch } : item)),
      );
    });
  }, [draftKey]);

  // While a send is in flight and untouched, the submitted snapshot is the
  // draft to keep: if it fails after unmount, the author still gets it back.
  useEffect(() => {
    return () => {
      const submitting = submittingSnapshotRef.current;
      const snapshot =
        submitting && editGenerationRef.current === submitting.editGeneration
          ? submitting.snapshot
          : {
              conversationId: draftKey,
              sendAttempt: retrySendRef.current,
              text: draftRef.current,
              caret: caretRef.current,
              pending: pendingRef.current,
              updatedAt: Date.now(),
            };
      writeComposerDraft({
        ...snapshot,
        updatedAt: Date.now(),
      });
    };
  }, [draftKey]);

  function setPendingValue(
    next: PendingAttachment[] | ((current: PendingAttachment[]) => PendingAttachment[]),
  ): void {
    const value = typeof next === "function" ? next(pendingRef.current) : next;
    pendingRef.current = value;
    setPendingState(value);
  }

  function updateDraft(nextDraft: string, nextCaret: number): void {
    detachSubmittingDraftForEdit();
    editGenerationRef.current += 1;
    draftRef.current = nextDraft;
    caretRef.current = nextCaret;
    setDraftState(nextDraft);
    setCaretState(nextCaret);
    patchComposerDraftText(draftKey, nextDraft, nextCaret);
  }

  function updateCaret(nextCaret: number): void {
    caretRef.current = nextCaret;
    setCaretState(nextCaret);
    patchComposerDraftText(draftKey, draftRef.current, nextCaret);
  }

  /** Editing starts the next message: the in-flight one no longer owns the
   *  draft, and a stale retry identity must not be reused for new content. */
  function detachSubmittingDraftForEdit(): void {
    retrySendRef.current = undefined;
    clearComposerSendAttempt(draftKey);
    const submitting = submittingSnapshotRef.current;
    if (!submitting) {
      return;
    }
    dropSentAttachmentsFromDraft(
      draftKey,
      submitting.snapshot.pending.map((item) => item.key),
    );
    submittingSnapshotRef.current = null;
    patchComposerDraftText(draftKey, draftRef.current, caretRef.current);
  }

  function submit(): void {
    // Refs, not state: a native composer sets the text it sends in the same
    // turn as the send.
    const content = draftRef.current.trim();
    const pending = pendingRef.current;
    // The send carries each attachment's compose-time label, the reference
    // its chip showed.
    const ready = pending
      .filter((p) => p.state === "done" && p.attachment)
      .map((p) => ({ ...p.attachment!, label: p.label }));
    if ((!content && ready.length === 0) || disabled) {
      return;
    }
    if (pendingAttachmentsBlockSend(pending)) {
      return;
    }
    const submittedPending = pending.filter((p) => p.state === "done" && p.attachment);
    const sentAttachmentIds = ready.map((item) => item.id);
    const logicalSend =
      retrySendRef.current ?? createLogicalSend(conversationId, threadRootId ?? null);
    retrySendRef.current = undefined;
    // A failed send restores what the author typed (the shorthand), never the
    // rewritten body.
    const submittedSnapshot: ComposerDraftSnapshot = {
      sendAttempt: logicalSend,
      conversationId: draftKey,
      text: content,
      caret: content.length,
      pending: submittedPending,
      updatedAt: Date.now(),
    };
    const editGeneration = editGenerationRef.current;
    submittingSnapshotRef.current = {
      snapshot: submittedSnapshot,
      editGeneration,
      sentAttachmentIds,
    };
    draftRef.current = "";
    caretRef.current = 0;
    pendingRef.current = [];
    setDraftState("");
    setCaretState(0);
    setPendingState([]);

    /** No-op once the next message has started; idempotent, as it clears the
     *  ref it tests. */
    const restoreDraft = (): void => {
      if (logicalSend.userId !== getCurrentUserId()) {
        return;
      }
      const submitting = submittingSnapshotRef.current;
      if (submitting?.editGeneration !== editGeneration) {
        return;
      }
      submittingSnapshotRef.current = null;
      retrySendRef.current = logicalSend;
      if (mountedRef.current) {
        draftRef.current = submittedSnapshot.text;
        caretRef.current = submittedSnapshot.caret;
        pendingRef.current = submittedSnapshot.pending;
        setDraftState(submittedSnapshot.text);
        setCaretState(submittedSnapshot.caret);
        setPendingState(submittedSnapshot.pending);
      }
      patchComposerDraftText(draftKey, submittedSnapshot.text, submittedSnapshot.caret);
      for (const item of submittedSnapshot.pending) {
        upsertComposerDraftAttachment(draftKey, item);
      }
      writeComposerDraft(submittedSnapshot);
    };
    const dispatch = (storedContent: string): void => {
      logicalSend.canonicalContent ??= storedContent;
      void send
        .mutateAsync({ content: logicalSend.canonicalContent, attachments: ready, logicalSend })
        .then(() => {
          if (logicalSend.userId !== getCurrentUserId()) {
            return;
          }
          const submitting = submittingSnapshotRef.current;
          if (submitting?.editGeneration !== editGeneration) {
            return;
          }
          clearComposerDraft(draftKey, { sentAttachmentIds });
          submittingSnapshotRef.current = null;
        })
        .catch(restoreDraft);
    };

    // A retry ships the body its first attempt stored.
    if (logicalSend.canonicalContent !== undefined) {
      dispatch(logicalSend.canonicalContent);
      return;
    }
    // The textarea keeps `@Alice`; the stored body spells every resolvable
    // shorthand as `<silverhand://agent/Alice>`, the only form that addresses
    // anyone. Code spans, quotes and link labels stay as written.
    if (canResolveShorthandNow(content, mentionSource.sourceKnown)) {
      dispatch(rewriteShorthandReferences(content, mentionSource.resolveShorthand));
      return;
    }
    // The body names someone but the authority is not known yet: settle it
    // first, or hand the draft back rather than store a body that addresses
    // nobody. The withheld arm has nothing to settle, so its handle stays
    // pending.
    void refetchMentionResolver()
      .then((resolver) => {
        if (!resolver) {
          restoreDraft();
          return;
        }
        dispatch(rewriteShorthandReferences(content, resolver));
      })
      .catch(restoreDraft);
  }

  /** Holds a card for each file before its bytes arrive, so it shows at once;
   *  `fillPending` then uploads into it. Returns the cards' keys, fewer than
   *  asked once the draft is full. */
  function reservePending(entries: { filename: string; contentType: string }[]): string[] {
    const slotsLeft = MAX_ATTACHMENTS - pendingRef.current.length;
    if (entries.length === 0 || slotsLeft <= 0) {
      return [];
    }
    // Detach before assigning labels, so new files number from #1 rather than
    // continuing the sent message's sequence.
    detachSubmittingDraftForEdit();
    editGenerationRef.current += 1;
    const items = entries.slice(0, slotsLeft).map((entry): PendingAttachment => ({
      key: `att-${crypto.randomUUID()}`,
      filename: entry.filename,
      size: 0,
      contentType: entry.contentType,
      state: "uploading",
      // The label is fixed the moment the file enters the draft, so the chip
      // is the reference the agent gets.
      label: assignAttachmentLabel(draftKey, entry.contentType),
    }));
    setPendingValue((curr) => [...curr, ...items]);
    for (const item of items) {
      upsertComposerDraftAttachment(draftKey, item);
    }
    return items.map((item) => item.key);
  }

  function fillPending(key: string, file: File): void {
    const item = pendingRef.current.find((candidate) => candidate.key === key);
    if (!item) {
      return;
    }
    const previewUrl = effectiveContentType(file).startsWith("image/")
      ? URL.createObjectURL(file)
      : undefined;
    void startComposerDraftAttachmentUpload({
      conversationId: draftKey,
      uploadChannelId: conversationId,
      attachmentKey: key,
      file,
      previewUrl,
      label: item.label,
    });
  }

  function failPending(key: string): void {
    const item = pendingRef.current.find((candidate) => candidate.key === key);
    if (!item) {
      return;
    }
    upsertComposerDraftAttachment(draftKey, {
      ...item,
      state: "error",
      error: t("messages.couldntReadFile"),
    });
  }

  function addFiles(files: FileList | File[]): void {
    const list = Array.from(files);
    const keys = reservePending(
      list.map((file) => ({ filename: file.name, contentType: effectiveContentType(file) })),
    );
    keys.forEach((key, index) => fillPending(key, list[index]!));
  }

  function removePending(key: string): void {
    detachSubmittingDraftForEdit();
    editGenerationRef.current += 1;
    setPendingValue((curr) => curr.filter((p) => p.key !== key));
    removeComposerDraftAttachment(draftKey, key);
  }

  return {
    draftKey,
    draft,
    caret,
    pending,
    sending: send.isPending,
    current: () => ({
      text: draftRef.current,
      caret: caretRef.current,
      pendingCount: pendingRef.current.length,
    }),
    updateDraft,
    updateCaret,
    submit,
    addFiles,
    reservePending,
    fillPending,
    failPending,
    removePending,
  };
}
