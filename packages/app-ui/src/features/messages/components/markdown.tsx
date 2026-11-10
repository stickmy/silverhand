import { getPlatform } from "@/platform";
import {
  type ComponentProps,
  type ReactNode,
  createContext,
  memo,
  useContext,
  useMemo,
} from "react";
import ReactMarkdown, { defaultUrlTransform } from "react-markdown";
import remarkBreaks from "remark-breaks";
import remarkGfm from "remark-gfm";
import { rehypeHighlightTerm } from "../model/rehype-highlight-term";
import { rehypeSharedHighlight } from "../model/rehype-shared-highlight";
import type { AgentDto, UserDto } from "@silverhand/shared";
import {
  exceedsRenderBudget,
  markdownByteLength,
  MESSAGE_RENDER_BUDGET_BYTES,
} from "../model/markdown-render-budget";
import { Mention } from "./mention";
import { MermaidDiagram } from "./mermaid-diagram";
import { ChannelRef, type ChannelReference } from "./channel-ref";
import { LocatorRef, LocatorRefStaticChip } from "./locator-ref";
import { ConversationRef, type WorkspaceChannelReference } from "./conversation-ref";
import { TaskRef, TaskRefStaticChip, type StaticTaskRefSummary } from "./task-ref";
import { classifyReferenceHref, isPluginLink, isReferenceScheme } from "../model/reference-href";
import {
  BARE_TASK_PREFIX,
  CHANNEL_PREFIX,
  CONVERSATION_PREFIX,
  isInternalChipScheme,
  MENTION_PREFIX,
  remarkRefs,
  TASK_PREFIX,
} from "../model/remark-refs";
import {
  mentionDisplayLabel,
  StaticChannelRef,
  StaticConversationRef,
  StaticMentionRef,
} from "./markdown-static-refs";
import { CodeBlock, extractLang, extractText, OversizedContent } from "./markdown-blocks";
import { useLocale } from "@/lib/i18n";

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

  const preContext = useMemo(
    () => ({ interactive, mermaidSvgByCode }),
    [interactive, mermaidSvgByCode],
  );

  if (oversizedBytes !== null) {
    return <OversizedContent content={content} bytes={oversizedBytes} />;
  }

  return (
    <div className="markdown-body">
      <PreContext.Provider value={preContext}>
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
            pre: MarkdownPre,
            code: ({ className, children, ...rest }) => (
              <code className={className} {...rest}>
                {children}
              </code>
            ),
          }}
        >
          {content}
        </ReactMarkdown>
      </PreContext.Provider>
    </div>
  );
}

/** What the `pre` override needs from the render. Read from context because
 *  the override must be a stable component: an inline arrow is a new component
 *  type on every render, so React would remount the whole block each time a
 *  message re-renders (a Mermaid diagram re-rendering, dropping its view and
 *  height, and pulling the message list off the bottom). */
const PreContext = createContext<{
  interactive: boolean;
  mermaidSvgByCode?: ReadonlyMap<string, string>;
}>({ interactive: true });

function MarkdownPre({ children }: { children?: ReactNode }): JSX.Element {
  const { interactive, mermaidSvgByCode } = useContext(PreContext);
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
}

export const Markdown = memo(MarkdownInner);
