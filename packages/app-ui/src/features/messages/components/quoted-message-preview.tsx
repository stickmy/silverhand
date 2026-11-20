import { useMemo } from "react";
import { ThreadIcon } from "@/icons";
import type { MessageDto } from "@silverhand/shared";
import { useMessage } from "@/features/messages/hooks";
import { useUsersById, makeUserNameResolver } from "@/features/users/hooks";
import { formatUserDisplayName } from "@/features/session/current-user";
import { Markdown } from "./markdown";
import { absoluteTime, RelativeTime } from "./relative-time";
import { t } from "@/lib/i18n";

interface QuotedMessagePreviewProps {
  /** The thread_anchor system message — its metadata carries the
   *  snapshot needed to render the original. For old rows without
   *  snapshot fields we lazily fetch the original via
   *  `quoted_message_id`. */
  quoteMessage: MessageDto;
  /** Click handler — caller can route a click to "jump back to the
   *  original message in the parent conversation". Undefined disables. */
  onJumpToOriginal?: () => void;
}

interface QuotedFields {
  content: string;
  fromKind: string | null;
  userId: string | null;
  agentName: string | null;
  createdAt: number;
}

function readSnapshot(metadata: MessageDto["metadata"]): QuotedFields | null {
  if (!metadata) return null;
  const meta = metadata as Record<string, unknown>;
  const content = typeof meta["quoted_content"] === "string" ? meta["quoted_content"] : null;
  const createdAt =
    typeof meta["quoted_created_at"] === "number" ? meta["quoted_created_at"] : null;
  if (!content || createdAt === null) return null;
  return {
    content,
    fromKind: typeof meta["quoted_from_kind"] === "string" ? meta["quoted_from_kind"] : null,
    userId:
      typeof meta["quoted_from_user_id"] === "string"
        ? (meta["quoted_from_user_id"] as string)
        : null,
    agentName:
      typeof meta["quoted_agent_name"] === "string" ? (meta["quoted_agent_name"] as string) : null,
    createdAt,
  };
}

function readQuotedMessageId(metadata: MessageDto["metadata"]): string | null {
  if (!metadata) return null;
  const v = (metadata as Record<string, unknown>)["quoted_message_id"];
  return typeof v === "string" ? v : null;
}

/**
 * Renders the message that anchored a thread child conversation as the first
 * row in the child's chat scroll area. Mirrors `MessageItem`'s frame
 * (white background, 2px subtle border, full width) so it reads as
 * "the original message" — a small "thread from" hint signals intent.
 *
 * Modern data: snapshot fields live in `metadata` directly, no extra
 * fetch. Legacy data (PR4 pre-fix): falls back to a lazy fetch of the
 * original message via `quoted_message_id`.
 */
export function QuotedMessagePreview({
  quoteMessage,
  onJumpToOriginal,
}: QuotedMessagePreviewProps): JSX.Element | null {
  const snapshot = readSnapshot(quoteMessage.metadata);

  // Legacy fallback — only fires when the snapshot is missing.
  const fallbackId = snapshot ? null : readQuotedMessageId(quoteMessage.metadata);
  const { data: fetched, isLoading: fetching } = useMessage(fallbackId);

  let fields: QuotedFields | null = snapshot;
  if (!fields && fetched) {
    fields = {
      content: fetched.message.content,
      fromKind: fetched.message.from_kind,
      userId: fetched.message.from_user_id,
      agentName: fetched.agent_name,
      createdAt: fetched.message.created_at,
    };
  }

  // Resolver for the snapshot's user_id — quoted snapshots store the
  // raw uuid even when the original message was viewable, so we lean on
  // the global directory here too (multi-human audit 2026-06-03
  // #task:857 P0.10).
  const usersById = useUsersById();
  const userResolver = useMemo(() => makeUserNameResolver(usersById), [usersById]);

  // No snapshot, no fallback id → can't render anything useful.
  if (!fields && !fallbackId) return null;
  // Loading the fallback — skip render until it resolves rather than
  // flash an empty card.
  if (!fields && fetching) return null;
  // Fallback finished but the message is gone (deleted parent etc.).
  if (!fields) return null;

  const isUser = fields.fromKind === "user";
  const displayName = isUser
    ? formatUserDisplayName(fields.userId, userResolver)
    : fields.agentName
      ? `@${fields.agentName}`
      : "@agent";

  return (
    <div
      className={
        "w-full border-2 border-border bg-bg-floating px-4 py-2 " +
        (onJumpToOriginal
          ? "cursor-pointer transition-colors hover:border-(--border-row-hover)"
          : "")
      }
      onClick={onJumpToOriginal}
      role={onJumpToOriginal ? "button" : undefined}
      tabIndex={onJumpToOriginal ? 0 : undefined}
      onKeyDown={
        onJumpToOriginal
          ? (e) => {
              if (e.key === "Enter" || e.key === " ") {
                e.preventDefault();
                onJumpToOriginal();
              }
            }
          : undefined
      }
    >
      <div className="mb-1 flex items-center gap-1 text-(length:--text-badge) uppercase tracking-wider text-text-muted">
        <ThreadIcon className="size-3" />
        <span>{t("messages.threadFrom")}</span>
      </div>
      <header className="mb-1 text-xs">
        <span className={"font-medium " + (isUser ? "text-text-primary" : "text-accent")}>
          {displayName}
        </span>
        <time
          dateTime={new Date(fields.createdAt).toISOString()}
          title={absoluteTime(fields.createdAt)}
          className="ml-2 text-text-muted"
        >
          <RelativeTime epochMs={fields.createdAt} />
        </time>
      </header>
      <div className="text-sm text-text-primary">
        <Markdown content={fields.content} />
      </div>
    </div>
  );
}
