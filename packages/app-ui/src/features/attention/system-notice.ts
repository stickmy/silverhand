import type { MessageAlert, MessageAuthorDto, NotificationDto } from "@silverhand/shared";
import { presentAuthor } from "@/features/messages/model/author-presentation";
import { readableExcerpt } from "./mention-banner";
import { t } from "@/lib/i18n";

/** An OS notification's lines. The system already names the app, so the title
 *  carries the subject, not "Silverhand". */
export interface SystemNotice {
  title: string;
  subtitle?: string;
  body?: string;
  /** Stacks a conversation's alerts into one group in Notification Center. */
  groupId?: string;
}

/** A message alert: who, where (channels only), what they said. */
export function alertNotice(alert: MessageAlert): SystemNotice {
  return {
    title: senderLabel(alert.sender) ?? t("attention.someone"),
    subtitle: alert.conversation_title
      ? t("attention.inChannel", { channel: alert.conversation_title })
      : undefined,
    body: alert.excerpt ? readableExcerpt(alert.excerpt) : undefined,
    groupId: alert.conversation_id,
  };
}

export function systemNoticeOf(n: NotificationDto, fallback: string): SystemNotice {
  const groupId = n.conversation_id ?? undefined;
  if (n.kind === "agent_start_failed") {
    const name = payloadString(n, "agent_name") ?? n.agent_id ?? t("attention.agent");
    const reason = payloadString(n, "reason");
    if (reason === "init_timeout") {
      return { title: t("attention.startTimeout", { name }), groupId };
    }
    if (reason === "runtime_crash") {
      const code = n.payload?.["exit_code"];
      return {
        title: t("attention.exited", { name }),
        body: code != null ? t("attention.exitCode", { code: String(code) }) : undefined,
        groupId,
      };
    }
    return {
      title: t("attention.didntStart", { name }),
      body: payloadString(n, "error") ?? t("attention.unknownError2"),
      groupId,
    };
  }
  return { title: fallback, groupId };
}

function senderLabel(sender: MessageAuthorDto | null): string | null {
  return sender && sender.status !== "unresolved" ? presentAuthor(sender).label : null;
}

export function payloadString(n: NotificationDto, key: string): string | null {
  const value = n.payload?.[key];
  return typeof value === "string" && value.trim() ? value.trim() : null;
}
