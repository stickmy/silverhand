import { getPlatform } from "@/platform";
import { useCallback, useEffect, useRef } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";
import {
  type MarkNotificationsReadInput,
  type NotificationDto,
  type UnreadUpdatedBadgePayload,
} from "@silverhand/shared";
import { api } from "@/lib/api";
import { onWsFrame, subscribeViewerScope, isCurrentViewerScope } from "@/lib/ws-client";
import { conversationRefQueryOptions } from "@/features/conversations/conversation-ref-query";
import { recordChannelWorkspace } from "@/features/channels/channel-workspace-index";
import type { MessageJumpTarget } from "@/features/channels/use-message-jump-controller";
import { showMentionBanner } from "./mention-banner";
import { payloadString, systemNoticeOf } from "./system-notice";
import { t } from "@/lib/i18n";

const unreadKey = ["notifications", "unread"] as const;
const listKey = ["notifications", "list"] as const;
const invalidationTimers = new Map<string, ReturnType<typeof setTimeout>>();

function invalidateQueryDebounced(
  qc: ReturnType<typeof useQueryClient>,
  queryKey: readonly unknown[],
): void {
  const key = JSON.stringify(queryKey);
  const existing = invalidationTimers.get(key);
  if (existing) clearTimeout(existing);
  const timer = setTimeout(() => {
    invalidationTimers.delete(key);
    void qc.invalidateQueries({ queryKey });
  }, 100);
  invalidationTimers.set(key, timer);
}

function isMessageUnreadUpdatedPayload(payload: unknown): boolean {
  return (
    typeof payload === "object" &&
    payload !== null &&
    typeof (payload as { conversation_id?: unknown }).conversation_id === "string"
  );
}

export interface UnreadCounts {
  total: number;
  by_workspace: Record<string, number>;
  /** Most-recent unread pointer per workspace/sidebar item — drives badge-click
   *  jump. `thread_root_id` is non-null when the newest unread lives in a reply
   *  cluster; the badge jump MUST carry it so the target opens inside its thread
   *  (a reply id anchored against the mainline stream 404s → the jump never
   *  reaches the thread observe path and unread never clears, #task:1519). The
   *  backend `latestUnreadByWorkspace` already resolves it from the message row. */
  latest_by_workspace: Record<
    string,
    { conversation_id: string; message_id: string; thread_root_id: string | null }
  >;
}

/**
 * Unread notification counts. Subscribes to the WS viewer scope and
 * invalidates on `notification` events — no more 5 s polling.
 */
export function useUnreadCounts() {
  const qc = useQueryClient();

  useEffect(() => {
    const sub = subscribeViewerScope();
    const unsub = onWsFrame((frame) => {
      if (frame.type === "event" && isCurrentViewerScope(frame.scope)) {
        if (
          frame.kind === "notification" ||
          (frame.kind === "unread_updated" && !isMessageUnreadUpdatedPayload(frame.payload))
        ) {
          invalidateQueryDebounced(qc, unreadKey);
        }
      } else if (frame.type === "snapshot_required" && isCurrentViewerScope(frame.scope)) {
        invalidateQueryDebounced(qc, unreadKey);
      }
    });
    return () => {
      sub.unsubscribe();
      unsub();
    };
  }, [qc]);

  return useQuery<UnreadCounts>({
    queryKey: unreadKey,
    queryFn: () => api<UnreadCounts>("/api/notifications/unread"),
    staleTime: 2_000,
  });
}

/** Full notification list — currently unused, but in place for the
 *  upcoming notifications panel. */
export function useNotifications(unread = false) {
  return useQuery<{ notifications: NotificationDto[] }>({
    queryKey: [...listKey, { unread }],
    queryFn: () =>
      api<{ notifications: NotificationDto[] }>(`/api/notifications${unread ? "?unread=1" : ""}`),
  });
}

export function useMarkNotificationsRead() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: MarkNotificationsReadInput) =>
      api<{ marked: number }>("/api/notifications/read", {
        method: "POST",
        body: JSON.stringify(input),
      }),
    onSuccess: (data) => {
      if (data.marked > 0) {
        qc.invalidateQueries({ queryKey: listKey });
      }
    },
  });
}

/**
 * Mount this once at the App root. Listens for WS viewer-scoped notification
 * events and surfaces each new one the way messaging apps do:
 *  - nothing while the viewer is looking at it: the window has focus and the
 *    conversation (and, for a reply, its thread) is on screen;
 *  - a message never toasts in the desktop app. A mention of the viewer goes
 *    to the OS even over a focused window (`force`); other kinds only when no
 *    window has focus (main gates them);
 *  - on iOS, which has no OS notifications, a mention of the viewer shows the
 *    native banner;
 *  - an agent start failure is an error, not a message: a sticky toast plus
 *    the OS notification, wherever the viewer is.
 *
 * Click navigation: the OS notification, the banner and the mention card all
 * route through `onJump`. When omitted, notifications still show but are
 * inert. `agent_start_failed` is not clickable (no message anchor).
 */
export function useNotificationToasts(
  viewing: { conversationId: string | null; threadRootId: string | null },
  onJump?: (target: MessageJumpTarget) => void,
) {
  const qc = useQueryClient();
  const { mutate: markRead } = useMarkNotificationsRead();
  const { data } = useQuery<{ notifications: NotificationDto[] }>({
    queryKey: [...listKey, { unread: true, toasts: true }],
    queryFn: () => api<{ notifications: NotificationDto[] }>("/api/notifications?unread=1"),
    staleTime: 2_000,
  });

  const seenIds = useRef<Set<string>>(new Set());
  const initialized = useRef(false);
  // Sticky start-failure toasts on screen. Each lives only while its notice is
  // unread, so a read from anywhere else (Activity, another device) takes it down.
  const stickyIds = useRef<Set<string>>(new Set());
  // Live notification rows the user can click on. Toast `onClick` reads
  // straight from this map; native-notification clicks come back from
  // main as ids and look up here too. We keep it as a ref instead of
  // closing over `data` so a stale closure (delivered to main yesterday,
  // clicked today) still resolves correctly.
  const clickableById = useRef<Map<string, NotificationDto>>(new Map());
  // Stable callback ref so the WS subscribe effect doesn't tear down
  // every time the parent's nav handler re-renders.
  const onJumpRef = useRef(onJump);
  useEffect(() => {
    onJumpRef.current = onJump;
  }, [onJump]);

  const handleClickById = useCallback(
    (notificationId: string): void => {
      const n = clickableById.current.get(notificationId);
      if (!n) return;
      const handler = onJumpRef.current;
      if (!handler) return;
      void resolveNotificationJumpTarget(n, qc).then((target) => {
        if (!target) return;
        handler(target);
      });
    },
    [qc],
  );

  // Toasts need the latest notification rows only when a new notification
  // arrives. `unread_updated` is a badge/read-cursor signal and would make
  // every channel switch refetch the toast list. The badge variant does,
  // however, carry the ids just marked read — forward those to main so the OS
  // notification center clears in step with in-app read state (#task:1431).
  // This is the single viewer-scope handler fanning the "notification read"
  // fact to the OS-dismiss projection; main never re-derives read state.
  useEffect(() => {
    const sub = subscribeViewerScope();
    const toastKey = [...listKey, { unread: true, toasts: true }] as const;
    const unsub = onWsFrame((frame) => {
      if (frame.type === "event" && isCurrentViewerScope(frame.scope)) {
        if (frame.kind === "notification") {
          invalidateQueryDebounced(qc, toastKey);
        } else if (
          frame.kind === "unread_updated" &&
          !isMessageUnreadUpdatedPayload(frame.payload)
        ) {
          const ids = (frame.payload as UnreadUpdatedBadgePayload).dismissed_notification_ids;
          if (Array.isArray(ids)) {
            const stringIds = ids.filter((id): id is string => typeof id === "string");
            if (stringIds.length > 0) {
              void getPlatform().notifications?.dismissSystemNotifications(stringIds);
            }
          }
        }
      } else if (frame.type === "snapshot_required" && isCurrentViewerScope(frame.scope)) {
        invalidateQueryDebounced(qc, toastKey);
      }
    });
    return () => {
      sub.unsubscribe();
      unsub();
    };
  }, [qc]);

  // Native-notification click bridge. Main process forwards the
  // `notificationId` originally passed to `showSystemNotification`
  // back here when the user clicks the OS-level alert.
  useEffect(() => {
    return getPlatform().notifications?.onNotificationClicked(handleClickById);
  }, [handleClickById]);

  // The native banner reports a tap by the notification's id.
  useEffect(() => {
    const banner = getPlatform().banner;
    if (!banner) {
      return;
    }
    let stop: (() => void) | null = null;
    let live = true;
    void banner
      .subscribe((event) => handleClickById(event.id))
      .then((unsubscribe) => {
        if (live) {
          stop = unsubscribe;
        } else {
          unsubscribe();
        }
      });
    return () => {
      live = false;
      stop?.();
    };
  }, [handleClickById]);

  useEffect(() => {
    if (!data) return;
    const notifications = data.notifications;

    const unreadIds = new Set(notifications.map((n) => n.id));
    for (const id of stickyIds.current) {
      if (!unreadIds.has(id)) {
        toast.dismiss(id);
        stickyIds.current.delete(id);
      }
    }

    if (!initialized.current) {
      // First load: existing notifications are seen without toasting, except an
      // unread agent start failure — it may have no Activity row (owner outside
      // the source conversation) and stays actionable until dismissed.
      for (const n of notifications) {
        if (n.kind === "agent_start_failed") continue;
        seenIds.current.add(n.id);
        if (isClickableKind(n.kind)) clickableById.current.set(n.id, n);
      }
      initialized.current = true;
    }

    for (const n of notifications) {
      // A start-failure row is refreshed in place (same id, newer created_at).
      const seenKey = n.kind === "agent_start_failed" ? `${n.id}@${n.created_at}` : n.id;
      if (seenIds.current.has(seenKey)) continue;
      seenIds.current.add(seenKey);
      if (isClickableKind(n.kind)) clickableById.current.set(n.id, n);

      if (n.kind === "agent_start_failed") {
        const label = buildToastLabel(n);
        // Sticky until the read is confirmed: that acknowledge lets the next
        // failure of this agent notify again. A failed read keeps it up for retry.
        stickyIds.current.add(n.id);
        toast.error(label, {
          id: n.id,
          duration: Infinity,
          action: {
            label: "Dismiss",
            onClick: (event) => {
              event.preventDefault();
              markRead(
                { id: n.id },
                {
                  onSuccess: () => toast.dismiss(n.id),
                  onError: () =>
                    toast.error(label, { id: n.id, description: "Couldn't dismiss. Try again." }),
                },
              );
            },
          },
        });
        void getPlatform().notifications?.showSystemNotification(systemNoticeOf(n, label));
        continue;
      }

      if (document.hasFocus() && isOnScreen(n, viewing)) {
        continue;
      }

      const isViewerMention = n.kind === "mention_self";
      if (isViewerMention) {
        showMentionBanner(
          {
            sender: n.sender ?? null,
            fallbackName: payloadString(n, "trigger_name") ?? t("attention.someone"),
            conversationTitle: payloadString(n, "conversation_title"),
            excerpt: payloadString(n, "mention_excerpt"),
          },
          n.id,
        );
      }
      void getPlatform().notifications?.showSystemNotification({
        ...systemNoticeOf(n, buildToastLabel(n)),
        force: isViewerMention,
        notificationId: isClickableKind(n.kind) ? n.id : undefined,
      });
    }

    // A toast is a transient SURFACING event, never an eliminate intent — so it
    // must not write `notifications.read_at`. Under Activity v1 that column is the
    // load-bearing directed-claim signal, so auto-reading on toast would silently
    // evict an unread @me from Activity before the user acted on it (surfaced ≠
    // eliminated). Re-toast dedup is owned by `seenIds` above, independent of
    // read-state, so the notification correctly stays unread until an ACTIVE
    // eliminate path clears it: visible read advancing the cursor, clicking the
    // toast/notification to jump to source, or the Activity Mark-as-read ✓.
  }, [data, viewing.conversationId, viewing.threadRootId, qc, handleClickById, markRead]);
}

/** Whether the notification's message is on screen: its conversation is the
 *  focused one and, for a reply, that reply's thread is open. */
function isOnScreen(
  n: NotificationDto,
  viewing: { conversationId: string | null; threadRootId: string | null },
): boolean {
  if (!n.conversation_id || n.conversation_id !== viewing.conversationId) {
    return false;
  }
  return !n.thread_root_id || n.thread_root_id === viewing.threadRootId;
}

/**
 * Whether a given notification kind has a meaningful click destination.
 * Both mention kinds open the message in the conversation where it was
 * authored: `mention_self` because the viewer is a participant there, and
 * `mention_external` because its own coordinates ARE the source's — the
 * viewer of that row is the mention's AUTHOR, so the anchor is the message
 * they wrote (backend `MentionExternalNotificationArgs`, shiki 2026-08-14
 * "option A"). The copy delivered to the mentioned agent is not the click
 * target — not because the author cannot reach it (since #task:2072 it lands
 * in the author↔agent direct line, which they read), but because the source is
 * where the surrounding context is. `agent_start_failed` has no message anchor
 * and is intentionally inert (user finds the failed agent via the regular
 * Members panel).
 */
function isClickableKind(kind: NotificationDto["kind"]): boolean {
  return kind === "mention_self" || kind === "mention_external";
}

/**
 * Map a notification row onto a `MessageJumpTarget`. The jump path
 * needs the *channel* id and an optional thread id; notifications
 * carry the conversation id directly, which may be a thread. Resolves
 * the conversation-ref via React Query's cache so repeated clicks
 * (and `useConversationById` reads of the same id) share the lookup.
 *
 * Returns `null` for kinds without a navigable target or when the
 * required ids are missing (e.g. malformed `mention_external` row).
 */
export async function resolveNotificationJumpTarget(
  n: NotificationDto,
  qc: ReturnType<typeof useQueryClient>,
): Promise<MessageJumpTarget | null> {
  if (n.kind === "mention_self") {
    if (!n.conversation_id || !n.message_id) return null;
    return resolveTargetFromConversationId(
      qc,
      n.workspace_id,
      n.conversation_id,
      n.message_id,
      n.thread_root_id ?? null,
    );
  }
  if (n.kind === "mention_external") {
    // Jump to the SOURCE message. For this kind the row's own coordinate group
    // already names the source (option A — see `isClickableKind` above), so this
    // branch and the `source_*` one below describe the same message for every row
    // written since `25e089d1` — NOT for every row in the table, see the previous
    // generation below; `thread_root_id` is joined from that same message, so a
    // mention posted as a cluster reply anchors inside its cluster.
    //
    // No generation of the producer has ever left the own group empty: before
    // `25e089d1` it named the then-destination copy (a local Mentions DM the
    // author was admitted to), and since then it names the source itself. So the
    // `source_*` branch below serves no population — it is a fail-close on
    // columns that are nullable for kinds with no anchor at all
    // (`agent_start_failed`), kept so a row that somehow arrives with half its
    // coordinates still navigates instead of going inert.
    if (n.conversation_id && n.message_id) {
      return resolveTargetFromConversationId(
        qc,
        n.workspace_id,
        n.conversation_id,
        n.message_id,
        n.thread_root_id ?? null,
      );
    }
    if (n.source_workspace_id && n.source_conversation_id && n.source_message_id) {
      return resolveTargetFromConversationId(
        qc,
        n.source_workspace_id,
        n.source_conversation_id,
        n.source_message_id,
        null,
      );
    }
    return null;
  }
  return null;
}

/**
 * Resolve a `(workspaceId, conversationId)` pair from a notification into a jump
 * target.
 *
 * SAME-GROUP CONTRACT (#task:1689). The two coordinates MUST come from the same
 * group of the same DTO: either `(workspace_id, conversation_id)` — the viewer's
 * own side — or `(source_workspace_id, source_conversation_id)` — the source
 * side. Backend never crosses them (all three notification insert sites pass the
 * workspace of the conversation they are inserting for), and this function
 * records the pair as an immutable channel → workspace binding, so a caller that
 * mixed one group's workspace with the other's conversation would write a WRONG
 * identity fact. `null` is skipped, never synthesized: `workspace_id` is nullable
 * in the DTO, and guessing a workspace is the one thing the attribution index
 * must never do.
 */
async function resolveTargetFromConversationId(
  qc: ReturnType<typeof useQueryClient>,
  workspaceId: string | null,
  conversationId: string,
  messageId: string,
  threadRootId: string | null,
): Promise<MessageJumpTarget> {
  // Attribution write-through: the notification itself is a carrier — it states
  // the workspace of the conversation it points at. Recording here (not only in
  // the ref fetcher below) is what closes the reply-cluster branch, which
  // returns without ever fetching a ref while still navigating the viewer into a
  // channel whose caches the removal contract has to be able to attribute.
  if (workspaceId) recordChannelWorkspace(conversationId, workspaceId);
  // New reply-cluster: the notification already carries the cluster root,
  // and `conversation_id` is the parent channel (cluster replies live in
  // the channel). Anchor straight to the message — no ref fetch, and never
  // a legacy thread hop.
  if (threadRootId) {
    return {
      workspaceId: workspaceId ?? undefined,
      channelId: conversationId,
      threadRootId,
      messageId,
    };
  }
  // THE shared conversation-ref read point — same key and same fetcher as
  // `useConversationById`, so a recently-resolved cross-workspace `?side` ref is
  // reused for free AND the attribution write-through happens exactly once,
  // wherever the fetch originates.
  const data = await qc.fetchQuery(conversationRefQueryOptions(conversationId)).catch(() => null);
  if (!data) {
    // Fetch failed — fall back to treating the id as a top-level
    // channel. Worse case the jump primer reports `not_found` and the
    // user sees an empty state; better than silently swallowing the click.
    return {
      workspaceId: workspaceId ?? undefined,
      channelId: conversationId,
      messageId,
    };
  }
  const ref = data.ref;
  const resolvedWorkspaceId = workspaceId ?? ref.workspace_id;
  return {
    workspaceId: resolvedWorkspaceId,
    channelId: ref.id,
    messageId,
  };
}

function buildToastLabel(n: NotificationDto): string {
  if (n.kind === "agent_start_failed") {
    const name =
      (n.payload?.["agent_name"] as string | undefined) ?? n.agent_id ?? t("attention.agent");
    const reason = n.payload?.["reason"] as string | undefined;
    if (reason === "init_timeout") return t("attention.startTimeout", { name });
    if (reason === "runtime_crash") {
      const code = n.payload?.["exit_code"];
      return code != null
        ? t("attention.exitedWithCode", { name, code: String(code) })
        : t("attention.exited", { name });
    }
    const error = (n.payload?.["error"] as string | undefined) ?? t("attention.unknownError");
    return t("attention.didntStartReason", { name, reason: error });
  }
  if (n.kind === "mention_external") {
    // `source_workspace_name` is a WORKSPACE name — a free-text label
    // (#task:1827), not `#`-addressable, so it renders bare. The `mention_self`
    // arm below reads `conversation_title` and keeps its `#`.
    const workspaceName = n.payload?.["source_workspace_name"] as string | undefined;
    return workspaceName
      ? t("attention.newMentionIn", { project: workspaceName })
      : t("attention.newMention");
  }
  if (n.kind === "mention_self") {
    const conversationTitle = n.payload?.["conversation_title"] as string | undefined;
    return conversationTitle
      ? t("attention.mentionedIn", { channel: conversationTitle })
      : t("attention.youWereMentioned");
  }
  return t("attention.newNotification");
}
