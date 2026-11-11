import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type {
  AgentDto,
  AttachmentSnapshot,
  MessageDto,
  ReactionAggregateDto,
  UserDto,
} from "@silverhand/shared";
import { decodeMessageSeq } from "@silverhand/shared";
import type { ChannelReference } from "./channel-ref";
import type { WorkspaceChannelReference } from "./conversation-ref";
import type { StaticTaskRefSummary } from "./task-ref";
import { MessageScreenshotView, type ScreenshotHeader } from "./message-screenshot-view";

// Cindy review of 2f8a2ca (P1 blocker): export view was reusing the
// full interactive `Markdown` component, leaking copy/expand buttons,
// `<a onClick=...>`, and Mention/channel-ref/conversation-ref/task-ref
// components (which run their own hooks) into the offscreen
// screenshot tree. The fix is `interactive={false}` on Markdown plus
// these contract assertions:
//
//   - export DOM has zero `<button>` and zero `[role="button"]`
//   - no code-block chrome class names (`.code-block__copy`,
//     `.code-block__expand`)
//   - plain external links collapse to span — no `href`, no `onclick`
//   - internal refs render as static chips matching the live trigger
//     visual contract, not the live ref components
//
// Tests SSR-render via renderToStaticMarkup so we assert on the HTML
// output directly (matching the markdown.test.ts pattern for this
// repo's node-env vitest setup).

function agent(id: string, overrides: Partial<AgentDto> = {}): AgentDto {
  return {
    id,
    name: id,
    display_name: id,
    runtime: "claude",
    home_dir: `/home/${id}`,
    role_summary: null,
    avatar_attachment_id: null,
    avatar_filename: null,
    avatar_size: null,
    avatar_content_type: null,
    avatar_blob_url_token: null,
    is_dm_agent: false,
    permissions: null,
    onboarded_at: null,
    created_at: 0,
    updated_at: 0,
    ...overrides,
  } as AgentDto;
}

function human(id: string, overrides: Partial<UserDto> = {}): UserDto {
  return {
    id,
    name: id,
    display_name: id,
    avatar_url: null,
    created_at: 0,
    updated_at: 0,
    ...overrides,
  } as UserDto;
}

function msg(
  id: string,
  seq: number,
  content: string,
  partial: {
    from_kind?: "user" | "agent";
    from_agent_id?: string | null;
    attachments?: AttachmentSnapshot[] | null;
    reactions?: ReactionAggregateDto[] | null;
  } = {},
): MessageDto {
  const fromKind = partial.from_kind ?? "agent";
  const fromAgentId = partial.from_agent_id ?? "a1";
  return {
    id,
    conversation_id: "c1",
    from_kind: fromKind,
    from_agent_id: fromAgentId,
    from_user_id: null,
    author:
      fromKind === "agent"
        ? {
            status: "active",
            kind: "agent",
            id: fromAgentId ?? "a1",
            name: "Agent",
            display_name: "Agent",
            avatar_url: null,
            runtime: "claude",
          }
        : {
            status: "active",
            kind: "user",
            id: "u1",
            name: "User",
            display_name: "User",
            avatar_url: null,
            runtime: null,
          },
    content,
    metadata: null,
    attachments: partial.attachments ?? null,
    seq: decodeMessageSeq(seq),
    created_at: 1_700_000_000_000 + seq * 1000,
    thread_root_id: null,
    linked_task: null,
    reactions: partial.reactions ?? null,
  };
}

function render(
  messages: MessageDto[],
  options: {
    compact?: boolean;
    header?: ScreenshotHeader | null;
    agentsByName?: ReadonlyMap<string, AgentDto>;
    humansByName?: ReadonlyMap<string, UserDto>;
    channelsByName?: ReadonlyMap<string, ChannelReference | null>;
    conversationRefsById?: ReadonlyMap<string, WorkspaceChannelReference>;
    taskRefsByNumber?: ReadonlyMap<number, StaticTaskRefSummary>;
    attachmentImageUrls?: ReadonlyMap<string, string>;
  } = {},
): string {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  // Author identity now comes from each message's `MessageAuthorDto` (built in
  // the `msg` fixture), not a `from_*_id` directory join. This map only backs
  // the inline @-mention pills' default (`agentsByName`).
  const agentsByName =
    options.agentsByName ?? new Map([["a1", agent("a1", { display_name: "Alice" })]]);
  return renderToStaticMarkup(
    React.createElement(
      QueryClientProvider,
      { client: qc },
      React.createElement(MessageScreenshotView, {
        messages,
        compact: options.compact,
        agentsByName,
        humansByName: options.humansByName,
        channelsByName: options.channelsByName,
        header:
          options.header === undefined ? { title: "design", counterpart: null } : options.header,
        conversationRefsById: options.conversationRefsById,
        taskRefsByNumber: options.taskRefsByNumber,
        attachmentImageUrls: options.attachmentImageUrls,
      }),
    ),
  );
}

describe("MessageScreenshotView — non-interactive contract", () => {
  it("uses a narrow export canvas without dropping selected content", () => {
    const messages = [msg("first", 1, "Start of transcript"), msg("last", 2, "End of transcript")];
    const mobile = render(messages, { compact: true });
    const desktop = render(messages);
    expect(mobile).toContain("width:360px");
    expect(desktop).toContain("width:760px");
    for (const output of [mobile, desktop]) {
      expect(output).toContain("Start of transcript");
      expect(output).toContain("End of transcript");
    }
  });
  it("renders the export channel header with the same channel symbol hook", () => {
    const html = render([msg("m1", 1, "hello")]);
    expect(html).toContain("data-conversation-title");
    // The channel `#` renders through the shared hash-icon hook (unified with
    // the sidebar `#` across every theme), not a text `#` glyph.
    expect(html).toContain("data-conversation-hash-icon");
    expect(html).toContain("design");
  });

  it("a DM header wears the counterpart's face instead of the channel hash", () => {
    const html = render([msg("m1", 1, "hello")], {
      header: {
        title: "Alice",
        counterpart: {
          status: "active",
          kind: "agent",
          id: "a1",
          name: "alice",
          display_name: "Alice",
          avatar_url: null,
          runtime: "codex",
        },
      },
    });
    expect(html).toContain("data-conversation-title");
    expect(html).not.toContain("data-conversation-hash-icon");
    expect(html).toContain("Alice");
  });

  it("omits the header when the pane names no conversation", () => {
    const html = render([msg("m1", 1, "hello")], { header: null });
    expect(html).not.toContain("data-conversation-title");
  });

  it("renders zero <button> elements for fenced code blocks", () => {
    const html = render([msg("m1", 1, "```ts\nconst answer = 42;\nconsole.log(answer);\n```")]);
    // Plain `<pre>` only; copy + expand buttons should be absent.
    expect(html).not.toMatch(/<button/);
    expect(html).not.toMatch(/role="button"/);
    expect(html).toContain("<pre");
    // hljs wraps tokens in spans, so `const answer` is split — assert
    // on individual tokens that survive highlighting.
    expect(html).toContain("answer");
    expect(html).toContain("42");
  });

  it("does not emit code-block chrome class names", () => {
    const html = render([msg("m1", 1, "```\necho hi\n```")]);
    expect(html).not.toContain("code-block__copy");
    expect(html).not.toContain("code-block__expand");
    expect(html).not.toContain("code-block__lang");
  });

  it("plain external links collapse to span without href / onclick", () => {
    const html = render([msg("m1", 1, "see [the docs](https://example.com)")]);
    // No href on the rendered link branch; no anchor element at all
    // for the static-link path. Static span with the text content.
    expect(html).toContain("the docs");
    expect(html).not.toMatch(/<a[^>]*href="https:\/\/example\.com"/);
    expect(html).not.toContain("onclick");
    // `markdown-link-static` is the marker class the export branch
    // applies — pinning it lets future drift trip a test instead of
    // silently re-exposing real anchors.
    expect(html).toContain("markdown-link-static");
  });

  it("unresolved @mention refs collapse to muted static text (no Mention component)", () => {
    const html = render([msg("m1", 1, "ping @bob about it")]);
    // `<Mention>` wraps a button + popover trigger; static path is a
    // bare span. So the dom should NOT contain any button-role
    // elements or the popover-content data attribute Radix injects.
    expect(html).toContain("@bob");
    expect(html).not.toMatch(/<button/);
    expect(html).not.toContain("data-radix");
  });

  it("resolved @mention refs use display labels without live popovers", () => {
    const html = render([msg("m1", 1, "ping @alice and @shiki")], {
      agentsByName: new Map([
        ["alice", agent("agent-alice", { name: "alice", display_name: "Alice" })],
      ]),
      humansByName: new Map([
        ["shiki", human("u-shiki", { name: "shiki", display_name: "shiki" })],
      ]),
    });
    expect(html).toContain("@Alice");
    expect(html).toContain("@shiki");
    expect(html).toContain('data-mention-kind="agent"');
    expect(html).toContain('data-mention-kind="human"');
    expect(html).not.toMatch(/<button/);
    expect(html).not.toContain("data-radix");
  });

  it("#conv: refs render as static chips (no ConversationRef hook)", () => {
    const html = render([msg("m1", 1, "see #conv:abcdefghijklmnop for details")]);
    expect(html).toContain("#conv:abcdefghijklmnop");
    expect(html).not.toContain('data-conversation-ref="abcdefghijklmnop"');
    expect(html).not.toMatch(/<button/);
  });

  it("resolved #conv: refs render human-readable labels in static export", () => {
    const refs = new Map<string, WorkspaceChannelReference>([
      [
        "abcdefghijklmnop",
        {
          id: "abcdefghijklmnop",
          title: "infra",
        },
      ],
    ]);
    const html = render([msg("m1", 1, "see #conv:abcdefghijklmnop")], {
      conversationRefsById: refs,
    });
    expect(html).toContain('data-conversation-ref="abcdefghijklmnop"');
    expect(html).toContain("infra");
    expect(html).not.toContain("#conv:abcdefghijklmnop");
    expect(html).not.toMatch(/<button/);
  });

  it("#task: refs render as static task chips (no live TaskRef hook)", () => {
    const html = render([msg("m1", 1, "fixed in #task:42")]);
    expect(html).not.toContain('data-task-ref="42"');
    expect(html).not.toContain('data-task-ref-state="static"');
    expect(html).toContain("#task:42");
    expect(html).not.toMatch(/<button/);
    expect(html).not.toMatch(/role="button"/);
  });

  it("#task: refs use preloaded static task titles in export", () => {
    const html = render([msg("m1", 1, "fixed in #task:42")], {
      taskRefsByNumber: new Map([[42, { task_number: 42, title: "Thread screenshot polish" }]]),
    });
    expect(html).toContain('data-task-ref="42"');
    // Resolved static chip shares the live `resolved` state so both the base
    // accent-link and poster's ink+gold-marker override apply to the export
    // (#task:1803) — the old export-only `static-resolved` matched neither and
    // rendered poster's chip as festive pink.
    expect(html).toContain('data-task-ref-state="resolved"');
    expect(html).toContain("#42");
    expect(html).toContain("Thread screenshot polish");
    expect(html).not.toContain("#task:42");
    expect(html).not.toMatch(/<button/);
    expect(html).not.toMatch(/role="button"/);
  });

  it("#<channel> refs render as static accent span (no ChannelRef component)", () => {
    const html = render([msg("m1", 1, "discuss in #design please")]);
    expect(html).toContain("#design");
    expect(html).not.toMatch(/<button/);
  });

  it("resolved #<channel> refs use the same static channel chip shape", () => {
    const html = render([msg("m1", 1, "discuss in #design please")], {
      channelsByName: new Map([["design", { id: "c-design", title: "design" }]]),
    });
    expect(html).toContain('data-channel-ref-state="resolved-no-handler"');
    expect(html).toContain('data-channel-ref-id="c-design"');
    expect(html).toContain("design");
    expect(html).not.toMatch(/<button/);
  });

  it("aggregate sanity — no interactive surfaces in a content with all ref types", () => {
    const html = render([
      msg(
        "m1",
        1,
        "recap @alice see #conv:abcdefghijklmnop and #task:7 in #general\n\n```sh\necho ok\n```\n\nDocs: [link](https://example.com)",
      ),
    ]);
    expect(html).not.toMatch(/<button/);
    expect(html).not.toMatch(/role="button"/);
    expect(html).not.toContain("onclick");
    expect(html).not.toContain("code-block__copy");
    expect(html).not.toContain("code-block__expand");
    // All identifiable text fragments are still present — the export
    // visual is intact, just stripped of every clickable surface.
    expect(html).toContain("@alice");
    expect(html).toContain("#conv:abcdefghijklmnop");
    expect(html).toContain("#task:7");
    expect(html).toContain("#general");
    // hljs splits the bash content; assert on a surviving token.
    expect(html).toContain("echo");
    expect(html).toContain("link");
  });

  it("strips XML-invalid control characters from static export text", () => {
    const html = render([msg("m1", 1, "ok. now I switch\u0008 away")]);
    expect(html).toContain("ok. now I switch away");
    expect(html).not.toContain("\u0008");
  });

  it("renders cached image attachments as static images", () => {
    const html = render(
      [
        msg("m1", 1, "attached", {
          attachments: [
            {
              id: "att-1",
              filename: "screenshot.png",
              size: 1234,
              content_type: "image/png",
            },
          ],
        }),
      ],
      { attachmentImageUrls: new Map([["att-1", "blob:screenshot"]]) },
    );
    expect(html).toContain("<img");
    expect(html).toContain('src="blob:screenshot"');
    expect(html).toContain("screenshot.png");
    expect(html).not.toContain("<figcaption");
    expect(html).not.toMatch(/<button/);
  });

  it("renders non-image attachments with the same static file chip shape as chat", () => {
    const html = render([
      msg("m1", 1, "attached", {
        attachments: [
          {
            id: "att-2",
            filename: "notes.txt",
            size: 512,
            content_type: "text/plain",
          },
        ],
      }),
    ]);
    expect(html).toContain("notes.txt");
    expect(html).toContain("512 B");
    expect(html).not.toMatch(/<button/);
  });

  // #task:1173 — reactions were invisible on screenshots because the export
  // renderer is a separate render path that never rendered them. These
  // parity assertions lock the structural fix: reactions visible in chat must
  // be visible in the export, still through a button/hook/portal-free chip.
  it("renders reaction chips in the export with emoji + count", () => {
    const html = render([
      msg("m1", 1, "shipped", {
        reactions: [
          { emoji: "🎉", count: 3, viewer_reacted: false },
          { emoji: "👀", count: 1, viewer_reacted: true },
        ],
      }),
    ]);
    expect(html).toContain("data-reaction-row");
    expect(html).toContain("data-reaction-chip");
    expect(html).toContain("🎉");
    expect(html).toContain("🎉, 3 reactions");
    expect(html).toContain("👀");
    expect(html).toContain("👀, 1 reaction");
    // Counts render as text alongside the glyph.
    expect(html).toContain(">3</span>");
    expect(html).toContain(">1</span>");
  });

  it("reaction chips in the export carry no interactive surface", () => {
    const html = render([
      msg("m1", 1, "shipped", {
        reactions: [{ emoji: "🎉", count: 2, viewer_reacted: true }],
      }),
    ]);
    // The live row uses <button> chips, a HoverCard portal, and an add
    // picker; the static export variant must have none of them.
    expect(html).not.toMatch(/<button/);
    expect(html).not.toMatch(/role="button"/);
    expect(html).not.toContain("data-radix");
    expect(html).not.toContain("data-reaction-add");
    expect(html).not.toContain("aria-pressed");
  });

  it("omits the reaction row entirely when a message has no reactions", () => {
    const html = render([msg("m1", 1, "no reactions here")]);
    expect(html).not.toContain("data-reaction-row");
    expect(html).not.toContain("data-reaction-chip");
  });
});
