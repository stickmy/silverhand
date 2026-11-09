import { ADDRESSABLE_ID_PATTERN } from "@silverhand/shared";
import { isAutolink } from "@silverhand/shared/reference-markdown";
import { AUTHORED_HREF_PROP, isReferenceScheme, PLUGIN_LINK_PROP } from "./reference-href";

export const MENTION_PREFIX = "silverhand-mention:";
export const CHANNEL_PREFIX = "silverhand-channel:";
export const CONVERSATION_PREFIX = "silverhand-conversation:";
export const TASK_PREFIX = "silverhand-task:";
// Bare `#<n>` task reference (no `#task:` sigil). Agents habitually write
// `#123` for a task; we resolve it against the workspace's task numbers and
// render a chip ONLY when a matching task exists. Unlike the explicit
// `#task:<n>` scheme — which keeps a muted chip even when unresolved — a bare
// `#<n>` is ambiguous (issue numbers, quantities, versions), so an unresolved
// one demotes to its literal text and leaves no chrome. The distinct scheme is
// what lets the `a` override pick the demote-to-text fallback for bare refs.
export const BARE_TASK_PREFIX = "silverhand-task-bare:";

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
export function remarkRefs({
  taskReferences = "resolve",
}: { taskReferences?: "resolve" | "raw" } = {}) {
  return (tree: unknown, file: { toString(): string }): void => {
    walk(tree as MdastNode, file.toString(), taskReferences);
  };
}

/** Every internal scheme the `a` override draws a chip for. The override
 *  requires {@link isPluginLink} provenance before it consumes any of them,
 *  because this pass is their only legitimate producer. */
export function isInternalChipScheme(href: string): boolean {
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
