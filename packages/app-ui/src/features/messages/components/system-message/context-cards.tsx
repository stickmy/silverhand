import { useState } from "react";
import { ChevronRightIcon, QuoteIcon, SpawnedIcon } from "@/icons";
import {
  type ChannelContextMetadata,
  type ChildContextMetadata,
  type MessageDto,
  type ThreadAnchorMetadata,
  type UserDto,
  readChannelContextMetadata,
  readChildContextMetadata,
  readThreadAnchorMetadata,
} from "@silverhand/shared";
import { makeUserNameResolver } from "@/features/users/hooks";
import { formatUserDisplayName } from "@/features/session/current-user";
import { Markdown } from "@/features/messages/components/markdown";
import { ConversationRef } from "@/features/messages/components/conversation-ref";
import type { WorkspaceChannelReference } from "@/features/messages/components/conversation-ref";
import type { ChannelReference } from "@/features/messages/components/channel-ref";
import { RelativeTime } from "@/features/messages/components/relative-time";
import { type RendererProps, useUserResolver, firstLine } from "./shared";
import { t } from "@/lib/i18n";

type ContextCardKind = "quoted" | "inherited" | "spawned";

interface ContextCardModel {
  kind: ContextCardKind;
  /** uppercase tag label: "Quoted" / "Inherited" / "Spawned-from" */
  tagLabel: string;
  /** Optional small phrase rendered before the source chip (e.g.
   *  "from " for `child_context`). Empty for the other kinds. */
  sourcePrefix: string;
  /** Conversation id of the source — rendered as a clickable
   *  `ConversationRef` chip so the reader can jump to the original
   *  conversation. Resolves through `useConversationIdentity`, so a
   *  cross-workspace channel AND a private line both name themselves. */
  sourceConversationId: string;
  /** Optional secondary label appended after the chip with a `·`
   *  separator (e.g. "shiki", "by Cindy"). Empty when the source
   *  has no companion identity to surface. */
  sourceAuthorLabel: string;
  /** raw body text rendered through Markdown when expanded */
  body: string;
  /** epoch ms of the original event, surfaced as relative time on
   *  the right of the header. `null` to suppress. */
  timestamp: number | null;
  /** when true, body opens expanded by default and the toggle is
   *  hidden — used for `thread_anchor` since the quoted text IS the
   *  point of the row. */
  defaultExpanded: boolean;
  /** when true, render the body inside a blockquote — currently only
   *  `thread_anchor` (visually echoes "this is quoted text"). */
  blockquote: boolean;
}

const CONTEXT_KIND_BORDER: Record<ContextCardKind, string> = {
  quoted: "border-l-accent",
  inherited: "border-l-success",
  spawned: "border-l-warning",
};

const CONTEXT_KIND_TAG_BG: Record<ContextCardKind, string> = {
  quoted: "bg-accent text-text-on-accent",
  inherited: "bg-success text-white",
  spawned: "bg-warning text-white",
};

const CONTEXT_KIND_TAG_ICON: Record<ContextCardKind, JSX.Element> = {
  quoted: <QuoteIcon className="size-3" />,
  inherited: <ChevronRightIcon className="size-3" />,
  spawned: <SpawnedIcon className="size-3" />,
};

function ContextCardRelativeTime({ epochMs }: { epochMs: number }): JSX.Element {
  return (
    <span className="text-text-muted">
      <RelativeTime epochMs={epochMs} />
    </span>
  );
}

/** Shared chrome for the three context-card subtypes. Renders
 *  registry-flagged kind tag + source chip + optional timestamp on
 *  the header line, then a collapsible body underneath. */
function ContextCardShell({
  model,
  conversationRefsById,
  onOpenConversationRef,
  channelsByName,
  onOpenChannelRef,
  humansByName,
}: {
  model: ContextCardModel;
  conversationRefsById?: Map<string, WorkspaceChannelReference>;
  onOpenConversationRef?: (id: string) => void;
  channelsByName?: Map<string, ChannelReference | null>;
  onOpenChannelRef?: (channelId: string) => void;
  humansByName?: Map<string, UserDto>;
}): JSX.Element {
  const [expanded, setExpanded] = useState(model.defaultExpanded);
  const collapsible = !model.defaultExpanded;
  const bodyMarkdown = (
    <Markdown
      content={model.body}
      conversationRefsById={conversationRefsById}
      onOpenConversationRef={onOpenConversationRef}
      channelsByName={channelsByName}
      onOpenChannelRef={onOpenChannelRef}
      humansByName={humansByName}
    />
  );

  return (
    <div
      data-context-card={model.kind}
      className={"my-1 border-l-2 bg-bg-floating px-2 py-2 " + CONTEXT_KIND_BORDER[model.kind]}
    >
      <div className="mb-1 flex min-w-0 flex-wrap items-center gap-1.5 text-xs text-text-muted">
        <span
          className={
            "inline-flex items-center gap-1 rounded-sm px-1.5 py-px text-(length:--text-badge) font-semibold uppercase tracking-wider " +
            CONTEXT_KIND_TAG_BG[model.kind]
          }
        >
          {CONTEXT_KIND_TAG_ICON[model.kind]}
          {model.tagLabel}
        </span>
        {model.sourcePrefix ? (
          <span className="text-text-secondary">{model.sourcePrefix}</span>
        ) : null}
        <ConversationRef
          id={model.sourceConversationId}
          conversation={conversationRefsById?.get(model.sourceConversationId)}
          onOpen={onOpenConversationRef}
        />
        {model.sourceAuthorLabel ? (
          <span className="text-text-secondary">{model.sourceAuthorLabel}</span>
        ) : null}
        {model.timestamp !== null ? <ContextCardRelativeTime epochMs={model.timestamp} /> : null}
        {collapsible ? (
          <button
            type="button"
            onClick={() => setExpanded((v) => !v)}
            className="ml-auto flex size-6 shrink-0 items-center justify-center rounded-(--control-radius) text-text-muted hover:bg-bg-hover hover:text-text-secondary"
            aria-expanded={expanded}
            aria-label={expanded ? t("messages.collapseContext") : t("messages.expandContext")}
            title={expanded ? t("messages.collapse") : t("messages.expand")}
            data-context-toggle={expanded ? "collapse" : "expand"}
          >
            <ChevronRightIcon
              className={"size-3.5 transition-transform " + (expanded ? "rotate-90" : "")}
            />
          </button>
        ) : null}
      </div>
      <div className={expanded ? "text-text-primary" : "truncate text-text-secondary"}>
        {expanded ? (
          model.blockquote ? (
            <blockquote className="border-l-2 border-border pl-2 text-text-secondary">
              {bodyMarkdown}
            </blockquote>
          ) : (
            bodyMarkdown
          )
        ) : (
          // Plain text first line so markdown blocks (lists, headings)
          // don't break the single-line clamp.
          <span>{firstLine(model.body)}</span>
        )}
      </div>
    </div>
  );
}

function ContextCardLegacyFallback({
  kind,
  tagLabel,
  message,
  conversationRefsById,
  onOpenConversationRef,
  channelsByName,
  onOpenChannelRef,
  humansByName,
}: {
  kind: ContextCardKind;
  tagLabel: string;
  message: MessageDto;
  conversationRefsById?: Map<string, WorkspaceChannelReference>;
  onOpenConversationRef?: (id: string) => void;
  channelsByName?: Map<string, ChannelReference | null>;
  onOpenChannelRef?: (channelId: string) => void;
  humansByName?: Map<string, UserDto>;
}): JSX.Element {
  return (
    <div
      data-context-card={kind}
      data-context-card-legacy="1"
      className={
        "mx-4 my-1 border-l-2 bg-bg-floating px-3 py-2 text-text-muted " + CONTEXT_KIND_BORDER[kind]
      }
    >
      <div className="mb-1">
        <span
          className={
            "inline-flex items-center gap-1 rounded-sm px-1.5 py-px text-(length:--text-badge) font-semibold uppercase tracking-wider " +
            CONTEXT_KIND_TAG_BG[kind]
          }
        >
          {tagLabel}
        </span>
      </div>
      <div className="italic">
        <Markdown
          content={message.content}
          conversationRefsById={conversationRefsById}
          onOpenConversationRef={onOpenConversationRef}
          channelsByName={channelsByName}
          onOpenChannelRef={onOpenChannelRef}
          humansByName={humansByName}
        />
      </div>
    </div>
  );
}

export function ThreadAnchorCard(props: RendererProps): JSX.Element {
  const meta = readThreadAnchorMetadata(
    (props.message.metadata ?? null) as Record<string, unknown> | null,
  );
  const userResolver = useUserResolver();
  if (!meta) {
    return <ContextCardLegacyFallback kind="quoted" tagLabel={t("messages.quoted")} {...props} />;
  }
  return (
    <ContextCardShell
      model={threadAnchorModel(meta, userResolver)}
      conversationRefsById={props.conversationRefsById}
      onOpenConversationRef={props.onOpenConversationRef}
      channelsByName={props.channelsByName}
      onOpenChannelRef={props.onOpenChannelRef}
      humansByName={props.humansByName}
    />
  );
}

function threadAnchorModel(
  meta: ThreadAnchorMetadata,
  userResolver?: ReturnType<typeof makeUserNameResolver>,
): ContextCardModel {
  let authorLabel: string;
  if (meta.quoted_from_kind === "agent") {
    authorLabel = meta.quoted_agent_name || "agent";
  } else if (meta.quoted_from_kind === "user") {
    authorLabel = formatUserDisplayName(meta.quoted_from_user_id, userResolver);
  } else {
    authorLabel = "system";
  }
  return {
    kind: "quoted",
    tagLabel: t("messages.quoted"),
    sourcePrefix: "",
    sourceConversationId: meta.quoted_conversation_id,
    sourceAuthorLabel: authorLabel,
    body: meta.quoted_content,
    timestamp: meta.quoted_created_at,
    defaultExpanded: true,
    blockquote: true,
  };
}

export function ChildContextCard(props: RendererProps): JSX.Element {
  const meta = readChildContextMetadata(
    (props.message.metadata ?? null) as Record<string, unknown> | null,
  );
  if (!meta) {
    return (
      <ContextCardLegacyFallback kind="inherited" tagLabel={t("messages.inherited")} {...props} />
    );
  }
  return (
    <ContextCardShell
      model={childContextModel(meta)}
      conversationRefsById={props.conversationRefsById}
      onOpenConversationRef={props.onOpenConversationRef}
      channelsByName={props.channelsByName}
      onOpenChannelRef={props.onOpenChannelRef}
      humansByName={props.humansByName}
    />
  );
}

function childContextModel(meta: ChildContextMetadata): ContextCardModel {
  return {
    kind: "inherited",
    tagLabel: t("messages.inherited"),
    sourcePrefix: "from ",
    sourceConversationId: meta.parent_conversation_id,
    sourceAuthorLabel: "",
    body: meta.summary,
    timestamp: null,
    defaultExpanded: false,
    blockquote: false,
  };
}

export function ChannelContextCard(props: RendererProps): JSX.Element {
  const meta = readChannelContextMetadata(
    (props.message.metadata ?? null) as Record<string, unknown> | null,
  );
  if (!meta) {
    return (
      <ContextCardLegacyFallback kind="spawned" tagLabel={t("messages.spawnedFrom")} {...props} />
    );
  }
  return (
    <ContextCardShell
      model={channelContextModel(meta, props.message.content)}
      conversationRefsById={props.conversationRefsById}
      onOpenConversationRef={props.onOpenConversationRef}
      channelsByName={props.channelsByName}
      onOpenChannelRef={props.onOpenChannelRef}
      humansByName={props.humansByName}
    />
  );
}

function channelContextModel(meta: ChannelContextMetadata, content: string): ContextCardModel {
  const agentLabel = meta.created_by_agent_name
    ? t("messages.byAgent", { name: meta.created_by_agent_name })
    : t("messages.byAnAgent");
  return {
    kind: "spawned",
    tagLabel: t("messages.spawnedFrom"),
    sourcePrefix: "",
    sourceConversationId: meta.source_conversation_id,
    sourceAuthorLabel: agentLabel,
    body: content,
    timestamp: null,
    defaultExpanded: false,
    blockquote: false,
  };
}
