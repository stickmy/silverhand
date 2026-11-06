import { getPlatform } from "@/platform";
import { CopyFeedbackIcon } from "./copy-feedback-icon";
import {
  type ComponentProps,
  memo,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import ReactMarkdown, { defaultUrlTransform } from "react-markdown";
import remarkBreaks from "remark-breaks";
import remarkGfm from "remark-gfm";
import { ChevronDownIcon, ChevronUpIcon, HashIcon } from "@/icons";
import { rehypeHighlightTerm } from "../model/rehype-highlight-term";
import { rehypeSharedHighlight } from "../model/rehype-shared-highlight";
import type { AgentDto, UserDto } from "@silverhand/shared";
import { ADDRESSABLE_ID_PATTERN } from "@silverhand/shared";
import { isAutolink } from "@silverhand/shared/reference-markdown";
import {
  exceedsRenderBudget,
  markdownByteLength,
  MESSAGE_RENDER_BUDGET_BYTES,
} from "../model/markdown-render-budget";
import { Mention } from "./mention";
import { MermaidDiagram } from "./mermaid-diagram";
import { ChannelRef, type ChannelReference } from "./channel-ref";
import { LocatorRef, LocatorRefStaticChip } from "./locator-ref";
import {
  ConversationRef,
  conversationIcon,
  faceLabel,
  shortConversationId,
  type WorkspaceChannelReference,
} from "./conversation-ref";
import { TaskRef, TaskRefStaticChip, type StaticTaskRefSummary } from "./task-ref";
import {
  AUTHORED_HREF_PROP,
  classifyReferenceHref,
  isPluginLink,
  isReferenceScheme,
  PLUGIN_LINK_PROP,
} from "../model/reference-href";
import { t, useLocale } from "@/lib/i18n";

interface MarkdownProps {
  content: string;
  /** Forwarded to inline `<Mention>` so the agent popover can show
   *  per-conversation presence + retry. Optional — system messages outside
   *  a chat context omit it. */
  conversationId?: string | null;
  /** Lookup by agent `name` (the handle used in @mentions). When
   *  absent, mentions render as plain accent-colored text without the
   *  popover. */
  agentsByName?: Map<string, AgentDto>;
  /** Object-URL map keyed by agent id. Threaded straight to inline
   *  `<Mention>` so `<AgentInfoPopover>` reuses the parent's batch
   *  fetch instead of firing one GET per `@mention`. Missing entries
   *  / `null` fall back to the runtime icon. */
  agentAvatarUrls?: Map<string, string | null>;
  /** Lookup by human user `name` (same @mention namespace as agents).
   *  The data layer guarantees `users.name` and `agents.name` are
   *  globally unique; rendering only consumes that invariant. */
  humansByName?: Map<string, UserDto>;
  /** Lookup by channel `title` for `#<channel_name>` refs in the
   *  message's workspace.
   *  - `ChannelReference` → unique match in this workspace
   *  - `null`             → ambiguity sentinel (multiple channels share
   *                         this title) — chip degrades to inert text
   *                         rather than guess a destination
   *  - missing entry      → no match in this workspace
   */
  channelsByName?: Map<string, ChannelReference | null>;
  /** Click handler for resolved channel refs. Receives the resolved
   *  channel id; the caller is responsible for binding it to the
   *  message's workspace via `nav.toChannel(workspaceId, channelId)`. */
  onOpenChannelRef?: (channelId: string) => void;
  /** Lookup by conversation id for `#conv:<id>` refs in the current
   *  workspace. Missing entries still render as clickable id refs when
   *  `onOpenConversationRef` is supplied. */
  conversationRefsById?: Map<string, WorkspaceChannelReference>;
  onOpenConversationRef?: (id: string) => void;
  /** Static task summaries for export / screenshot rendering. Live
   *  `TaskRef` resolves with hooks; non-interactive export trees consume
   *  this map instead so `#task:<n>` can still show `#<n> · title`
   *  without mounting query/popover components. */
  taskRefsByNumber?: Map<number, StaticTaskRefSummary>;
  /** Task descriptions and message excerpts keep task-number text literal. */
  taskReferences?: "resolve" | "raw";
  /** Set to `false` for export / screenshot rendering. When false:
   *  - fenced code blocks render as a bare `<pre>` (no copy / expand
   *    buttons, no overflow useEffect)
   *  - plain external links collapse to a styled `<span>` (no href,
   *    no onClick, no IPC)
   *  - internal refs (mention / channel / #conv / #task) render as
   *    static output without mounting `<Mention>`, `<ChannelRef>`,
   *    `<ConversationRef>`, or live `<TaskRef>` — those components run
   *    hooks / queries that have no place inside an offscreen screenshot
   *    tree. `#task` uses a pure static chip with the same theme hook.
   *
   *  Default `true` keeps the in-channel behaviour exactly as before.
   *  This is consumed by `MessageScreenshotView` (Cindy's review of
   *  2f8a2ca: the export view was leaking interactive chrome into
   *  the captured PNG). */
  interactive?: boolean;
  /** Pre-rendered Mermaid SVGs keyed by trimmed diagram source, for the
   *  export/screenshot path (#task:1322). When a mermaid fence's source is
   *  present here, the non-interactive export tree injects the finished SVG
   *  instead of falling back to source — so a shared image shows the rendered
   *  diagram. Absent key (diagram failed / timed out pre-render, or interactive
   *  path) → source fallback, exactly as before. */
  mermaidSvgByCode?: ReadonlyMap<string, string>;
  /** In-conversation header find (#task:1335): when set (non-empty), every
   *  case-insensitive literal occurrence of this term in the rendered body is
   *  wrapped in `<mark class="search-hit">` by a rehype plugin. Absent / empty
   *  → the exact pre-find pipeline (no extra plugin, no re-processing). */
  searchQuery?: string;
  /** How much markdown this surface is willing to render (see
   *  `markdown-render-budget.ts`). `'message'` (default) bounds the body at the
   *  message content cap and falls back to a plain-text preview above it;
   *  `'unbounded'` renders whatever it is handed — for callers already bounded
   *  upstream, i.e. the workspace file preview. */
  renderBudget?: "message" | "unbounded";
}

const MENTION_PREFIX = "silverhand-mention:";
const CHANNEL_PREFIX = "silverhand-channel:";
const CONVERSATION_PREFIX = "silverhand-conversation:";
const TASK_PREFIX = "silverhand-task:";
// Bare `#<n>` task reference (no `#task:` sigil). Agents habitually write
// `#123` for a task; we resolve it against the workspace's task numbers and
// render a chip ONLY when a matching task exists. Unlike the explicit
// `#task:<n>` scheme — which keeps a muted chip even when unresolved — a bare
// `#<n>` is ambiguous (issue numbers, quantities, versions), so an unresolved
// one demotes to its literal text and leaves no chrome. The distinct scheme is
// what lets the `a` override pick the demote-to-text fallback for bare refs.
const BARE_TASK_PREFIX = "silverhand-task-bare:";

/**
 * Renders message body as Markdown.
 *
 * Security: react-markdown defaults to escaping raw HTML. We do *not*
 * register `rehype-raw`, so agent-provided `<script>` etc. is rendered
 * as text. Links and code blocks are explicitly overridden so we never
 * navigate the renderer window or hand untrusted text to the shell.
 *
 * Mentions: a message names a participant with a canonical reference
 * (`<silverhand://agent/Alice>`, #task:2000) — a real Markdown autolink, so it
 * arrives here as a `link` node and is recognized by `reference-href.ts`. The
 * plain `@handle` form is a permanent DISPLAY alias: `remarkMentions` rewrites
 * those text spans into link nodes carrying the internal `silverhand-mention:`
 * scheme. Both arms render the same `<Mention>` chip, so the agent map only
 * needs to be threaded one place, and neither decides who the message addresses
 * — that is read server-side from the canonical form alone, and it drives
 * notification / unread, thread follow + mention CC, and an agent's stdin
 * framing. On the MAIN LINE it does not decide recipients at all: a channel
 * message fans out to members and may auto-start them whatever the body says.
 * In a THREAD it can: `applyThreadFollowEffects` inserts a follower row for a
 * referenced channel member and the same transaction then reads the updated
 * follower set to plan this reply (@Cindy, 2026-08-13).
 */
function MarkdownInner({
  content,
  conversationId,
  agentsByName,
  agentAvatarUrls,
  humansByName,
  channelsByName,
  onOpenChannelRef,
  conversationRefsById,
  onOpenConversationRef,
  taskRefsByNumber,
  taskReferences = "resolve",
  interactive = true,
  mermaidSvgByCode,
  searchQuery,
  renderBudget = "message",
}: MarkdownProps): JSX.Element {
  // Oversized guard: a multi-MB body (legacy DB rows predating the send-side
  // byte cap) pins the renderer process in remark/rehype, so anything over this
  // surface's budget falls back to a bounded plain-text preview. The budget is
  // per consumer (`markdown-render-budget.ts`): a chat body is bounded by the
  // message cap, the workspace file preview is bounded by the daemon's read cap
  // and so opts out here rather than inheriting a chat limit.
  const budget = renderBudget === "unbounded" ? null : MESSAGE_RENDER_BUDGET_BYTES;
  useLocale();
  const oversizedBytes = useMemo(
    () => (exceedsRenderBudget(content, budget) ? markdownByteLength(content) : null),
    [content, budget],
  );

  // Plugin instance is recreated per render but is a thin closure;
  // remark instantiates the transformer once per `process` call so
  // the cost is negligible. Memoizing it doesn't help because react-
  // markdown also memoizes the pipeline by reference equality of the
  // plugins array.
  const plugins = useMemo<NonNullable<ComponentProps<typeof ReactMarkdown>["remarkPlugins"]>>(
    () => [remarkGfm, remarkBreaks, [remarkRefs, { taskReferences }]],
    [taskReferences],
  );

  // Keyword highlight is opt-in: only when the header find is active do we
  // append `rehypeHighlightTerm` after `rehype-highlight`. Recreated when the
  // term changes so react-markdown re-processes (that's the point — the marks
  // must track the query); when there's no term we return the stable
  // highlight-only array so ordinary rendering is byte-identical to before.
  const trimmedTerm = searchQuery?.trim() ?? "";
  const rehypePlugins = useMemo<NonNullable<ComponentProps<typeof ReactMarkdown>["rehypePlugins"]>>(
    () =>
      trimmedTerm
        ? [rehypeSharedHighlight, [rehypeHighlightTerm, { term: trimmedTerm }]]
        : [rehypeSharedHighlight],
    [trimmedTerm],
  );

  if (oversizedBytes !== null) {
    return <OversizedContent content={content} bytes={oversizedBytes} />;
  }

  return (
    <div className="markdown-body">
      <ReactMarkdown
        remarkPlugins={plugins}
        rehypePlugins={rehypePlugins}
        // Whitelist our internal schemes — react-markdown's default
        // sanitizer drops unknown URL schemes (turning `href` into an
        // empty string), which would cause the `a` override below to
        // miss the prefix check and render an inert link.
        //
        // `silverhand://` (the canonical reference form, #task:2000) is
        // admitted at SCHEME level and case-insensitively, which is wider
        // than what draws a chip on purpose: provenance (`remarkRefs`
        // stamps the bare autolinks) and the strict parser then decide
        // chip vs. inert text, so everything the two of them reject must
        // still reach an arm that refuses to make it clickable.
        // Sanitizing it to `''` here would instead route it to the
        // plain-link fallthrough, whose click handler hands the href to the
        // OS.
        //
        // Restricted to `href` so the scheme reaches exactly the one
        // override that consumes it: `![x](silverhand://agent/Alice)` has
        // no arm, so its `src` keeps going through the default sanitizer.
        urlTransform={(url, key) =>
          url.startsWith(MENTION_PREFIX) ||
          url.startsWith(CHANNEL_PREFIX) ||
          url.startsWith(CONVERSATION_PREFIX) ||
          url.startsWith(TASK_PREFIX) ||
          url.startsWith(BARE_TASK_PREFIX) ||
          (key === "href" && isReferenceScheme(url))
            ? url
            : defaultUrlTransform(url)
        }
        components={{
          a: ({ href, children, node, ...rest }) => {
            // An internal chip scheme has exactly one producer — `remarkRefs`,
            // below — so a link that carries one WITHOUT its provenance flag was
            // hand-written in the body: `[anything](silverhand-mention:Alice)`
            // would otherwise draw Alice's chip over the author's own text, the
            // same spoof ruling C removed from the reference family. Inert text,
            // and above all never the plain-link arm, which would hand an
            // internal scheme to the OS opener.
            if (typeof href === "string" && isInternalChipScheme(href) && !isPluginLink(node)) {
              return <span className="text-text-muted">{children}</span>;
            }
            // Export / screenshot path: every reachable href shape
            // collapses to a styled span. Internal-scheme chips keep
            // their accent class so visual fidelity is preserved, but
            // none of them mount the live components (which run their
            // own hooks / queries). Plain links lose `href` entirely
            // — no clickable handler, no `target=_blank`, no role.
            if (!interactive) {
              if (typeof href === "string") {
                if (href.startsWith(MENTION_PREFIX)) {
                  const name = href.slice(MENTION_PREFIX.length);
                  const mentionAgent = agentsByName?.get(name);
                  const mentionHuman = humansByName?.get(name);
                  return (
                    <StaticMentionRef
                      name={name}
                      label={mentionDisplayLabel(mentionAgent, mentionHuman)}
                      resolved={!!mentionAgent || !!mentionHuman}
                      humanId={mentionHuman?.id}
                    />
                  );
                }
                // Canonical reference (#task:2000): the same chip as the alias
                // above, resolved within the kind the destination names — an
                // `agent/` reference must not resolve against the human map, or
                // the export would show a chip for a principal the author did
                // not address. Only a bare autolink gets here as `reference`
                // (`remarkRefs` stamps those); every other spelling of the scheme
                // falls to the inert arm below, because the chip replaces the
                // label and would drop text the author wrote from the export
                // while the raw body keeps it.
                const staticRef = classifyReferenceHref(href, node);
                if (staticRef.arm === "reference") {
                  const { kind, handle } = staticRef.reference;
                  const refAgent = kind === "agent" ? agentsByName?.get(handle) : undefined;
                  const refHuman = kind === "human" ? humansByName?.get(handle) : undefined;
                  return (
                    <StaticMentionRef
                      name={handle}
                      label={mentionDisplayLabel(refAgent, refHuman)}
                      resolved={!!refAgent || !!refHuman}
                      humanId={refHuman?.id}
                    />
                  );
                }
                if (staticRef.arm === "locator") {
                  // A locator in an export is a coordinate, never a control —
                  // see `LocatorRefStaticChip`.
                  return <LocatorRefStaticChip>{children}</LocatorRefStaticChip>;
                }
                if (staticRef.arm !== "other") {
                  // Either malformed, or written in a spelling display refuses.
                  // Both show the authored text and nothing else — never a
                  // fall-through to the link arm.
                  return <span className="text-text-muted">{children}</span>;
                }
                if (href.startsWith(CHANNEL_PREFIX)) {
                  const name = href.slice(CHANNEL_PREFIX.length);
                  return <StaticChannelRef name={name} channel={channelsByName?.get(name)} />;
                }
                if (href.startsWith(CONVERSATION_PREFIX)) {
                  const id = href.slice(CONVERSATION_PREFIX.length);
                  return (
                    <StaticConversationRef id={id} conversation={conversationRefsById?.get(id)} />
                  );
                }
                if (href.startsWith(TASK_PREFIX)) {
                  const raw = href.slice(TASK_PREFIX.length);
                  const taskNumber = Number.parseInt(raw, 10);
                  if (/^[1-9]\d*$/.test(raw) && Number.isFinite(taskNumber)) {
                    return (
                      <TaskRefStaticChip
                        taskNumber={taskNumber}
                        task={taskRefsByNumber?.get(taskNumber)}
                      />
                    );
                  }
                  return <span>{children}</span>;
                }
                if (href.startsWith(BARE_TASK_PREFIX)) {
                  // Bare `#<n>` in the export path: only render a chip when
                  // the export builder pre-resolved this number to a task.
                  // Otherwise keep the literal text — a bare number that
                  // isn't a task must not sprout chrome in a screenshot.
                  const raw = href.slice(BARE_TASK_PREFIX.length);
                  const taskNumber = Number.parseInt(raw, 10);
                  const resolved =
                    /^[1-9]\d*$/.test(raw) && Number.isFinite(taskNumber)
                      ? taskRefsByNumber?.get(taskNumber)
                      : null;
                  return resolved ? (
                    <TaskRefStaticChip taskNumber={taskNumber} task={resolved} />
                  ) : (
                    <span>{children}</span>
                  );
                }
              }
              return (
                <span className="markdown-link-static text-accent underline underline-offset-2">
                  {children}
                </span>
              );
            }
            if (typeof href === "string" && href.startsWith(MENTION_PREFIX)) {
              const name = href.slice(MENTION_PREFIX.length);
              const mentionAgent = agentsByName?.get(name);
              // `undefined` = parent didn't thread the map (no batch
              // context — popover falls back to its own fetch).
              // `null`     = batched parent looked it up; the agent
              //              has no avatar yet, render runtime icon.
              const mentionAvatarObjectUrl =
                agentAvatarUrls && mentionAgent
                  ? (agentAvatarUrls.get(mentionAgent.id) ?? null)
                  : undefined;
              return (
                <Mention
                  name={name}
                  agent={mentionAgent}
                  agentAvatarObjectUrl={mentionAvatarObjectUrl}
                  human={humansByName?.get(name)}
                  conversationId={conversationId ?? null}
                />
              );
            }
            // Canonical reference (#task:2000). Same chip as the alias arm
            // above — a reader must not have to know which spelling the author
            // used — but resolved only within the kind the destination names,
            // and only for a bare autolink: chipping
            // `[Alice](silverhand://human/shiki)` would replace the author's
            // label with a different principal's chip, and chipping any
            // labelled form would show as addressed something the addressing
            // side does not record as a reference.
            const liveRef = classifyReferenceHref(href, node);
            if (liveRef.arm === "reference") {
              const { kind, handle } = liveRef.reference;
              const refAgent = kind === "agent" ? agentsByName?.get(handle) : undefined;
              const refHuman = kind === "human" ? humansByName?.get(handle) : undefined;
              const refAvatarObjectUrl =
                agentAvatarUrls && refAgent
                  ? (agentAvatarUrls.get(refAgent.id) ?? null)
                  : undefined;
              return (
                <Mention
                  name={handle}
                  agent={refAgent}
                  agentAvatarObjectUrl={refAvatarObjectUrl}
                  human={refHuman}
                  conversationId={conversationId ?? null}
                />
              );
            }
            if (liveRef.arm === "locator") {
              // A container coordinate (#task:2120). It resolves itself — no map
              // is threaded in, because a locator can name ANY workspace and the
              // render context's maps are scoped to this message's one.
              return <LocatorRef locator={liveRef.locator}>{children}</LocatorRef>;
            }
            if (liveRef.arm !== "other") {
              // Our scheme, but either a malformed destination or a spelling
              // display refuses: show the authored text and nothing else.
              // Falling through to the plain-link arm would hand an internal
              // scheme to `openExternal` on click, and drawing a chip would
              // claim an addressee this body never names to the addressing side.
              return <span className="text-text-muted">{children}</span>;
            }
            if (typeof href === "string" && href.startsWith(CHANNEL_PREFIX)) {
              const name = href.slice(CHANNEL_PREFIX.length);
              return (
                <ChannelRef
                  name={name}
                  channel={channelsByName?.get(name)}
                  onOpen={onOpenChannelRef}
                />
              );
            }
            if (typeof href === "string" && href.startsWith(CONVERSATION_PREFIX)) {
              const id = href.slice(CONVERSATION_PREFIX.length);
              return (
                <ConversationRef
                  id={id}
                  conversation={conversationRefsById?.get(id)}
                  onOpen={onOpenConversationRef}
                />
              );
            }
            if (typeof href === "string" && href.startsWith(TASK_PREFIX)) {
              // The tokenizer guarantees the slice is `[1-9]\d*` —
              // positive integer with no leading zero. parseInt + finite
              // check is defensive belt-and-suspenders for the case
              // where someone hand-authors `[label](silverhand-task:0)`
              // through agent-supplied markdown the tokenizer never
              // touched. Render the original child text inertly rather
              // than fall through to the plain-link branch — that branch
              // calls `getPlatform().files.openExternal(href)` on click,
              // which would shell out our internal scheme.
              const raw = href.slice(TASK_PREFIX.length);
              const taskNumber = Number.parseInt(raw, 10);
              if (/^[1-9]\d*$/.test(raw) && Number.isFinite(taskNumber) && taskNumber > 0) {
                return <TaskRef taskNumber={taskNumber} />;
              }
              // Pathological — render as plain text so we don't crash
              // on a malformed href and don't hand our internal scheme
              // to the OS handler.
              return <span>{children}</span>;
            }
            if (typeof href === "string" && href.startsWith(BARE_TASK_PREFIX)) {
              // Bare `#<n>`: same resolve path as `#task:<n>`, but the
              // `bare` flag makes TaskRef demote to its literal text
              // (no chip) for the no-context / loading / missing states.
              // Only a resolved-present task yields a chip. The tokenizer
              // guarantees `[1-9]\d*`; parse defensively as above.
              const raw = href.slice(BARE_TASK_PREFIX.length);
              const taskNumber = Number.parseInt(raw, 10);
              if (/^[1-9]\d*$/.test(raw) && Number.isFinite(taskNumber) && taskNumber > 0) {
                return <TaskRef taskNumber={taskNumber} bare />;
              }
              return <span>{children}</span>;
            }
            return (
              <a
                {...rest}
                href={href}
                onClick={(e) => {
                  e.preventDefault();
                  if (href) void getPlatform().files.openExternal(href);
                }}
              >
                {children}
              </a>
            );
          },
          pre: ({ children }) => {
            // Mermaid fences auto-render to a diagram on the live path
            // (#task:1322). The export/screenshot path (`interactive=false`)
            // keeps the source as a plain code block instead — the capture is
            // synchronous and mermaid's async render wouldn't finish in time,
            // so showing the (always-correct) source beats a blank diagram.
            // Case-insensitive: `mermaid` is a proper noun (capital M) and
            // agents commonly write ```Mermaid — a stray capital shouldn't
            // silently demote the diagram to a plain code block.
            if (extractLang(children)?.toLowerCase() === "mermaid") {
              if (interactive) {
                return <MermaidDiagram code={extractText(children)} />;
              }
              // Export path: use the SVG pre-rendered before capture
              // (#task:1322) if this diagram succeeded; otherwise keep the
              // always-correct source. The SVG was produced by the same
              // `renderMermaidSvg` entry + theme as the live component, so the
              // shared image matches what's on screen. It's already
              // DOMPurify-sanitized (securityLevel:'strict'), safe to inject.
              const svg = mermaidSvgByCode?.get(extractText(children).trim());
              if (svg) {
                return (
                  <div
                    className="mermaid-diagram"
                    data-mermaid-state="ok"
                    dangerouslySetInnerHTML={{ __html: svg }}
                  />
                );
              }
              return (
                <div className="code-block">
                  <pre>{children}</pre>
                </div>
              );
            }
            return interactive ? (
              <CodeBlock>{children}</CodeBlock>
            ) : (
              // Export path: bare `<pre>`. No copy / expand button,
              // no overflow useEffect — the captured screenshot
              // shows the code as-is and any chrome would just be
              // noise. We still keep the `code-block` class so
              // existing CSS (font, padding) lands.
              <div className="code-block">
                <pre>{children}</pre>
              </div>
            );
          },
          code: ({ className, children, ...rest }) => (
            <code className={className} {...rest}>
              {children}
            </code>
          ),
        }}
      >
        {content}
      </ReactMarkdown>
    </div>
  );
}

export const Markdown = memo(MarkdownInner);

/** The label a mention chip shows: `display_name` when the principal has one,
 *  else the handle (shiki's ruling — a chip always reads as the person, not the
 *  address). Shared by both spellings so an alias and a canonical reference to
 *  the same principal are indistinguishable on screen. Kind scoping happens at
 *  the call site; at most one of the two is ever present. */
function mentionDisplayLabel(
  agent: AgentDto | undefined,
  human: UserDto | undefined,
): string | null {
  return (
    agent?.display_name?.trim() || agent?.name || human?.display_name?.trim() || human?.name || null
  );
}

function StaticMentionRef({
  name,
  label,
  resolved,
  humanId,
}: {
  name: string;
  label: string | null;
  resolved: boolean;
  humanId?: string;
}): JSX.Element {
  if (!resolved) {
    return <span className="text-text-muted">@{name}</span>;
  }
  return (
    <span
      className="rounded-(--chip-radius) px-1 -mx-0.5 font-medium text-accent"
      aria-label={`@${name}`}
      data-mention-kind={humanId ? "human" : "agent"}
      data-mention-id={humanId}
    >
      @{label ?? name}
    </span>
  );
}

function StaticChannelRef({
  name,
  channel,
}: {
  name: string;
  channel: ChannelReference | null | undefined;
}): JSX.Element {
  if (!channel) {
    return <>{`#${name}`}</>;
  }
  return (
    <span
      className={
        "inline-flex items-baseline gap-0.5 rounded-(--chip-radius) px-1 -mx-0.5 font-medium text-accent" +
        (channel.closed_at != null ? " opacity-60" : "")
      }
      title={`#${channel.title}`}
      data-channel-ref-state="resolved-no-handler"
      data-channel-ref-id={channel.id}
    >
      <HashIcon className="size-3 shrink-0 self-center" />
      {channel.title}
    </span>
  );
}

function StaticConversationRef({
  id,
  conversation,
}: {
  id: string;
  conversation: WorkspaceChannelReference | undefined;
}): JSX.Element {
  if (!conversation) {
    return <>{`#conv:${id}`}</>;
  }

  const label = conversation
    ? conversationDisplayName(conversation)
    : `#conv:${shortConversationId(id)}`;
  // `channel` is not a guess: the parameter's TYPE says this ref came from a
  // workspace-scoped list ({@link WorkspaceChannelReference} carries the
  // argument), so a direct pair never reaches it. Same face, same word, same
  // glyph as the live chip — one authority (#task:1373).
  const icon = conversationIcon(conversation ? "channel" : undefined);
  return (
    <span
      className="inline-flex max-w-55 items-center gap-0.5 rounded-(--chip-radius) bg-accent-soft px-1.5 -mx-0.5 font-medium text-accent"
      aria-label={
        conversation ? `${faceLabel("channel")} ${label}` : t("messages.conversationId", { id })
      }
      data-conversation-ref={id}
    >
      {icon}
      <span className={conversation ? "truncate" : "truncate font-mono"}>{label}</span>
    </span>
  );
}

function conversationDisplayName(conversation: WorkspaceChannelReference): string {
  return conversation.title;
}

// Walks the mdast tree and replaces inline `@handle`, `#handle`, and
// `#conv:<conversation_id>` substrings inside text nodes with link nodes
// that carry our internal silverhand-* URL schemes.
// Skipped containers: `inlineCode`, `code`, and existing `link`
// nodes (so a hand-written `[#foo](https://…)` stays intact, and a
// literal `@x` inside `\`@bob\`` doesn't fire). Heading text is
// already extracted into `heading` nodes by the time we run, so a
// markdown `#` line never reaches us.
//
// It also does the one thing only a remark pass CAN do for canonical references
// (#task:2001): decide whether a `silverhand://` link was written as a bare
// autolink. That question is answered by slicing the source, and this is the last
// point where the source is in hand — the `a` override downstream sees a hast
// property. Hence the second parameter, and hence `markReferenceAutolink`
// stamping the destinations it admits.
function remarkRefs({ taskReferences = "resolve" }: { taskReferences?: "resolve" | "raw" } = {}) {
  return (tree: unknown, file: { toString(): string }): void => {
    walk(tree as MdastNode, file.toString(), taskReferences);
  };
}

/** Every internal scheme the `a` override draws a chip for. The override
 *  requires {@link isPluginLink} provenance before it consumes any of them,
 *  because this pass is their only legitimate producer. */
function isInternalChipScheme(href: string): boolean {
  return (
    href.startsWith(MENTION_PREFIX) ||
    href.startsWith(CHANNEL_PREFIX) ||
    href.startsWith(CONVERSATION_PREFIX) ||
    href.startsWith(TASK_PREFIX) ||
    href.startsWith(BARE_TASK_PREFIX)
  );
}

interface MdastNode {
  type: string;
  value?: string;
  url?: string;
  children?: MdastNode[];
  data?: { hProperties?: Record<string, unknown> };
  position?: {
    start: { offset?: number | undefined };
    end: { offset?: number | undefined };
  };
}

/** A link node this pass vouches for: either one it built out of prose, or a
 *  reference it admitted. The flag rides in mdast metadata, which no message
 *  body can write — see {@link PLUGIN_LINK_PROP}. */
function stampPluginLink(node: MdastNode): MdastNode {
  node.data = {
    ...node.data,
    hProperties: { ...node.data?.hProperties, [PLUGIN_LINK_PROP]: true },
  };
  return node;
}

/** A chip link built from prose: internal scheme, literal authored text as the
 *  visible child, provenance stamped. */
function pluginLink(url: string, text: string): MdastNode {
  return stampPluginLink({
    // mdast `link` uses `url`, not `href`. mdast-util-to-hast maps
    // it to the `href` attribute on the rendered `<a>`.
    type: "link",
    url,
    children: [{ type: "text", value: text }],
  });
}

/**
 * Carry the destination's authored bytes past hast serialization, which rewrites
 * them — see {@link AUTHORED_HREF_PROP} for why a locator cannot be parsed from
 * the `href` the `a` override receives.
 *
 * Only an AUTHORED destination gets this: a link this pass built out of prose
 * (`pluginLink`) has a synthesized internal-scheme url and no author behind it, so
 * stamping one would make the prop's name false. Reading it is therefore a strict
 * question — "did the author write this destination?" — with `null` for everything
 * else.
 */
function stampAuthoredHref(node: MdastNode, url: string): void {
  node.data = {
    ...node.data,
    hProperties: { ...node.data?.hProperties, [AUTHORED_HREF_PROP]: url },
  };
}

function walk(node: MdastNode, source: string, taskReferences: "resolve" | "raw"): void {
  if (!node.children || node.children.length === 0) return;
  const next: MdastNode[] = [];
  for (const child of node.children) {
    if (child.type === "link") {
      markReferenceAutolink(child, source);
      next.push(child);
      continue;
    }
    if (child.type === "inlineCode" || child.type === "code") {
      next.push(child);
      continue;
    }
    if (
      child.type === "text" &&
      typeof child.value === "string" &&
      (child.value.includes("@") || child.value.includes("#"))
    ) {
      pushSplit(child.value, next, taskReferences);
      continue;
    }
    walk(child, source, taskReferences);
    next.push(child);
  }
  node.children = next;
}

/**
 * Stamp provenance on a reference destination when — and only when — it was
 * written as a bare autolink.
 *
 * The syntax predicate is shared's {@link isAutolink}, the SAME function the
 * addressing side (`extractMessageReferences`) calls, so "was this written as
 * `<dest>`?" has one answer in this product rather than two implementations that
 * can drift (@Cindy, #task:2001).
 * What is deliberately NOT shared is position policy: the extractor refuses code
 * spans and blockquotes, while a quoted message must keep looking like the
 * message, so this pass stamps references wherever prose can appear. `code` /
 * `inlineCode` cannot contain a `link` node at all, so nothing extra is needed to
 * keep a quoted example from chipping.
 *
 * A destination we do NOT stamp keeps its authored href and therefore lands on
 * the inert arm — visible text, no chip, not clickable. The flag lives in mdast
 * metadata rather than in the href precisely so that an author cannot write it.
 *
 * Both stamps are applied together and to the same set of destinations, so
 * "vouched for" and "here are its authored bytes" can never disagree about a node
 * (#task:2120).
 */
function markReferenceAutolink(node: MdastNode, source: string): void {
  if (typeof node.url !== "string" || !isReferenceScheme(node.url)) return;
  if (!isAutolink(node, source)) return;
  stampPluginLink(node);
  stampAuthoredHref(node, node.url);
}

// Leading-boundary validation is handled in `canStartRef`. It keeps
// email addresses (`me@example.com`), paths (`a/@b`), and URL
// fragments (`page#anchor`) out, while still allowing slash-separated
// live refs such as `#task:317/#task:318`. Handle character class
// mirrors `CreateAgentInputSchema` / `CreateChannelInputSchema` in
// `packages/shared/src/api/agent.ts` / `conversation.ts` (alphanumerics, `-`, `_`).
// Conversation ids are generated from 12 random bytes as base64url,
// yielding exactly 16 chars. The trailing negative lookahead prevents
// matching a valid-looking prefix inside a longer id-like token.
//
// Typed sigil grammar: `#<type>:<value>`. We currently recognize:
//  - `#conv:<id>`  — a conversation id, whose domain is the ONE constant
//                    `ADDRESSABLE_ID_PATTERN` (#task:2021 B3). Interpolated
//                    rather than re-spelled here on purpose: the same token
//                    has to mean the same thing in a delivery header and in
//                    prose, so a scanner that were WIDER than the header codec
//                    would draw a chip for a string no `target=` can carry
//                    (live address in one read face, dead text in the other).
//  - `#task:<n>`   — workspace-scoped positive integer task number;
//                    `[1-9]\d*` rejects zero AND leading zeros so the
//                    chip never points at a row that can't exist. The
//                    `(?!\w)` boundary prevents `#task:12abc` from
//                    swallowing `12` as a task — same strict tail
//                    Cindy review of `80f544e` flagged on the URL
//                    parser.
//  - `#<n>`        — BARE task number (no `#task:` sigil). Agents write
//                    `#123` to mean task 123. Routed through the distinct
//                    BARE_TASK_PREFIX so an unresolved `#123` demotes to
//                    its literal text instead of showing a chip (see the
//                    scheme comment above). This branch is LAST so the
//                    explicit `#conv:` / `#task:` / `#<channel>` forms
//                    always win at a shared `#`. The numeric range
//                    starts at 11 — `#1` … `#10` collide with the
//                    Markdown editorial habit of numbering steps
//                    `#1, #2, #3`, so those stay plain text. Explicit
//                    `#task:1` … `#task:10` still resolve normally.
// The channel branch explicitly excludes `#conv:` and `#task:`
// prefixes via negative lookahead so a malformed `#conv:bad` /
// `#task:abc` doesn't fall back to matching `#conv` / `#task` as a
// stray channel reference. A digit-first `#<n>` never collides with the
// alpha-first channel branch.
const REF_RE = new RegExp(
  `(?:#conv:(${ADDRESSABLE_ID_PATTERN})(?![A-Za-z0-9_-])` +
    `|#task:([1-9]\\d*)(?!\\w)` +
    `|@([a-zA-Z][a-zA-Z0-9_-]*)` +
    `|#(?!conv:|task:)([a-zA-Z][a-zA-Z0-9_-]*)` +
    `|#((?:1[1-9]|[2-9]\\d|[1-9]\\d{2,}))(?!\\w))`,
  "g",
);

function pushSplit(text: string, out: MdastNode[], taskReferences: "resolve" | "raw"): void {
  let last = 0;
  let lastRefEnd = -1;
  let m: RegExpExecArray | null;
  let any = false;
  while ((m = REF_RE.exec(text)) !== null) {
    if (!canStartRef(text, m.index, lastRefEnd)) {
      continue;
    }
    any = true;
    if (m.index > last) {
      out.push({ type: "text", value: text.slice(last, m.index) });
    }
    const refEnd = m.index + m[0].length;
    const conversationId = m[1];
    if (conversationId) {
      out.push(pluginLink(`${CONVERSATION_PREFIX}${conversationId}`, `#conv:${conversationId}`));
      last = refEnd;
      lastRefEnd = refEnd;
      continue;
    }
    const taskNumber = m[2];
    if (taskNumber) {
      out.push(
        taskReferences === "raw"
          ? { type: "text", value: m[0] }
          : pluginLink(`${TASK_PREFIX}${taskNumber}`, `#task:${taskNumber}`),
      );
      last = refEnd;
      lastRefEnd = refEnd;
      continue;
    }
    const mentionName = m[3];
    if (mentionName) {
      out.push(pluginLink(`${MENTION_PREFIX}${mentionName}`, `@${mentionName}`));
      last = refEnd;
      lastRefEnd = refEnd;
      continue;
    }
    const channelName = m[4];
    if (channelName) {
      out.push(pluginLink(`${CHANNEL_PREFIX}${channelName}`, `#${channelName}`));
      last = refEnd;
      lastRefEnd = refEnd;
      continue;
    }
    // Bare `#<n>` task reference. The `text` child keeps the literal
    // `#<n>` so the demote-to-text fallback (unresolved / no context)
    // reproduces the original prose verbatim.
    const bareTaskNumber = m[5]!;
    out.push(
      taskReferences === "raw"
        ? { type: "text", value: m[0] }
        : pluginLink(`${BARE_TASK_PREFIX}${bareTaskNumber}`, `#${bareTaskNumber}`),
    );
    last = refEnd;
    lastRefEnd = refEnd;
  }
  if (!any) {
    out.push({ type: "text", value: text });
    return;
  }
  if (last < text.length) {
    out.push({ type: "text", value: text.slice(last) });
  }
}

function canStartRef(text: string, index: number, lastRefEnd: number): boolean {
  if (index === 0) return true;
  const prev = text[index - 1]!;
  if (prev === "/") {
    // Let slash act as a compact separator between already-live refs:
    // `#task:317/#task:318`, `#conv:<id>/#task:318`, etc. Without a
    // live ref immediately before the slash, keep treating `/@x` and
    // `/#task:1` as path/URL text.
    return lastRefEnd === index - 1;
  }
  return !/[\w]/.test(prev);
}

const OVERSIZED_PREVIEW_CHARS = 4000;

/** Bounded plain-text rendering for bodies over the render budget (legacy
 *  oversized rows). Never touches the markdown pipeline; full content stays
 *  retrievable via copy. */
function OversizedContent({ content, bytes }: { content: string; bytes: number }): JSX.Element {
  const [copied, setCopied] = useState(false);
  const onCopy = useCallback(async () => {
    try {
      await navigator.clipboard.writeText(content);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      /* clipboard denied; ignore */
    }
  }, [content]);
  const label =
    bytes >= 1024 * 1024
      ? `${(bytes / (1024 * 1024)).toFixed(1)} MB`
      : `${Math.round(bytes / 1024)} KB`;
  return (
    <div className="markdown-body">
      <div className="flex items-center gap-2 text-xs opacity-60">
        <span>
          {label} {t("messages.tooLargeToRender")}
        </span>
        <button type="button" onClick={onCopy} className="underline hover:opacity-80">
          {copied ? t("messages.copied") : t("messages.copyFullText")}
        </button>
      </div>
      <pre className="whitespace-pre-wrap wrap-break-word">
        {content.slice(0, OVERSIZED_PREVIEW_CHARS)}
        {"\n…"}
      </pre>
    </div>
  );
}

function CodeBlock({ children }: { children: React.ReactNode }): JSX.Element {
  const [copied, setCopied] = useState(false);
  const [expanded, setExpanded] = useState(false);
  const [overflowing, setOverflowing] = useState(false);
  const preRef = useRef<HTMLPreElement>(null);

  const lang = extractLang(children);

  const onCopy = useCallback(async () => {
    const text = extractText(children);
    if (!text) return;
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      /* clipboard denied; ignore */
    }
  }, [children]);

  // Detect whether the content actually exceeds the cap so we only
  // surface the Expand affordance when it would do something. Re-runs
  // on every render — the body content is the children prop, which
  // changes when streaming append-only edits land.
  useEffect(() => {
    const el = preRef.current;
    if (!el) return;
    setOverflowing(el.scrollHeight > el.clientHeight + 1);
  }, [children, expanded]);

  return (
    <div className={"code-block" + (expanded ? " code-block--expanded" : "")}>
      <div className="code-block__header">
        <span className="code-block__lang">{lang ?? "text"}</span>
        <button
          type="button"
          onClick={onCopy}
          className="code-block__copy"
          title={copied ? t("messages.copied") : t("messages.copyCode")}
          aria-label={t("messages.copyCode")}
        >
          {/* Icon only — the header strip is chrome, and a word next to a
              glyph that already says "copy" spends label width on nothing
              (shiki #task:1835). The Check/Copy swap carries the copied
              confirmation on its own; the accessible name stays on
              `aria-label` (constant, so screen readers announce the action)
              while `title` carries the transient "Copied" state. */}
          <CopyFeedbackIcon copied={copied} />
        </button>
      </div>
      <pre ref={preRef}>{children}</pre>
      {overflowing || expanded ? (
        <button
          type="button"
          onClick={() => setExpanded((value) => !value)}
          className="code-block__expand"
          aria-label={expanded ? t("messages.collapseCode") : t("messages.expandCode")}
          aria-expanded={expanded}
          title={expanded ? t("messages.collapseCode") : t("messages.expandCode")}
        >
          {expanded ? (
            <ChevronUpIcon size={16} aria-hidden="true" />
          ) : (
            <ChevronDownIcon size={16} aria-hidden="true" />
          )}
        </button>
      ) : null}
    </div>
  );
}

/** Pull `language-xxx` off the (singular) child `<code>` element that
 *  react-markdown places inside every fenced `pre`. Returns null for
 *  unfenced blocks or when the agent omitted the info string. */
function extractLang(children: React.ReactNode): string | null {
  if (
    children &&
    typeof children === "object" &&
    "props" in children &&
    typeof (children as { props?: { className?: unknown } }).props?.className === "string"
  ) {
    const cls = (children as { props: { className: string } }).props.className;
    const m = /language-([\w+-]+)/.exec(cls);
    if (m) return m[1] ?? null;
  }
  return null;
}

function extractText(node: React.ReactNode): string {
  if (typeof node === "string") return node;
  if (typeof node === "number") return String(node);
  if (Array.isArray(node)) return node.map(extractText).join("");
  if (
    node &&
    typeof node === "object" &&
    "props" in node &&
    (node as { props?: { children?: React.ReactNode } }).props?.children !== undefined
  ) {
    return extractText((node as { props: { children: React.ReactNode } }).props.children);
  }
  return "";
}
