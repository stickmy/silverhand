import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { StaticRouter } from "react-router-dom/server";
import { describe, expect, it, vi } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { AgentDto, TaskSummaryDto, UserDto } from "@silverhand/shared";
import { decodeMessageSeq } from "@silverhand/shared";
import { Markdown } from "./markdown";
import { MESSAGE_RENDER_BUDGET_BYTES } from "../model/markdown-render-budget";
import { SystemMessage } from "./system-message";
import type { ChannelReference } from "./channel-ref";
import type { WorkspaceChannelReference } from "./conversation-ref";
import { AUTHORED_HREF_PROP, PLUGIN_LINK_PROP } from "../model/reference-href";
import { TaskRefContextProvider } from "@/features/tasks/components/task-ref-context";
import { taskBoardCacheScope, taskByNumberKey } from "@/features/tasks/hooks/use-task-by-number";
import { setCurrentUserId } from "@/features/session/current-user";

// Static markup tests do not execute navigation. Interactive route behavior
// is tested against the real history store in navigation/provider.test.ts.
vi.mock("@/navigation/provider", () => ({
  useNavigationDriver: () => ({
    go: vi.fn(),
    back: vi.fn(),
    selectTab: vi.fn(),
    tab: "chat",
    canBack: false,
    registerLeaveGuard: () => () => {},
  }),
  useNavigationReader: () => () => ({
    driver: { go: vi.fn(), selectTab: vi.fn(), openSection: vi.fn() },
    location: { pathname: "/chat", search: "" },
  }),
}));

// Tests pass actor ids through SystemMessage rendering and assert the
// renderer maps the viewer's id to "@you". Under the post pre-ship
// rewrite identity model there is no implicit alias for legacy
// literals like `'local'` — `isCurrentUserId(id)` is true only when
// `id === currentUserId`. Seed `currentUserId = 'local'` once for
// the SSR test environment so fixtures using `actor_user_id: 'local'`
// keep rendering as "@you" without re-introducing the alias.
setCurrentUserId("local", "you");

function render(content: string, refs?: Map<string, WorkspaceChannelReference>): string {
  // ConversationRef now
  // calls `useConversationById` so it can resolve cross-workspace
  // refs that the synchronous `conversationRefsById` map can't see.
  // That hook requires a QueryClient context — wrap every render
  // in one. SSR doesn't drive the query lifecycle so the prop path
  // (`conversationRefsById` resolved entries) still produces the
  // resolved markup the assertions check; missing entries land in
  // `loading` and the popover stays skeleton (caller asserts
  // chip-level data attributes only).
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return renderToStaticMarkup(
    React.createElement(
      QueryClientProvider,
      { client: qc },
      React.createElement(Markdown, {
        content,
        conversationRefsById: refs,
        onOpenConversationRef: () => undefined,
      }),
    ),
  );
}

function renderWithHumans(content: string, humansByName: Map<string, UserDto>): string {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return renderToStaticMarkup(
    React.createElement(
      QueryClientProvider,
      { client: qc },
      React.createElement(
        StaticRouter,
        { location: "/" },
        React.createElement(Markdown, {
          content,
          humansByName,
        }),
      ),
    ),
  );
}

/**
 * Render a Markdown subtree wrapped with the React Query provider plus
 * an optional `TaskRefContext`. SSR rendering does not actually run the
 * `useQuery` lifecycle (no fetch happens, isLoading stays true on first
 * render), so for the tokenizer-shape assertions we only need the
 * wrapper to satisfy the hook's required context. PR-4 commit 3 will
 * add a hook-level test that exercises resolved/missing/loading paths
 * via a query-cache pre-seed.
 */
function renderWithTaskCtx(
  content: string,
  conversationId: string | null,
  onOpenTask?: (taskId: string) => void,
): string {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return renderToStaticMarkup(
    React.createElement(
      QueryClientProvider,
      { client: qc },
      React.createElement(
        TaskRefContextProvider,
        { value: { workspaceId: null, conversationId, onOpenTask } },
        React.createElement(Markdown, { content }),
      ),
    ),
  );
}

describe("Markdown soft breaks", () => {
  it("renders a single newline from the composer as a visible line break", () => {
    const html = render("line one\nline two");

    expect(html).toContain("line one<br/>");
    expect(html).toContain("line two");
  });

  it("preserves soft breaks around inline refs instead of requiring a blank line", () => {
    const html = render("@alice\n#channel");

    expect(html).toContain("<br/>");
    expect(html).toContain("@alice");
    expect(html).toContain("#channel");
  });

  it("keeps GFM setext heading semantics for a line containing ---", () => {
    const html = render("这是第一行\n---\n这是第三行");

    expect(html).toContain("<h2>这是第一行</h2>");
    expect(html).toContain("这是第三行");
  });

  it("keeps thematic breaks when they are separated as Markdown blocks", () => {
    const html = render("before\n\n---\n\nafter");

    expect(html).toContain("<p>before</p>");
    expect(html).toContain("<hr/>");
    expect(html).toContain("<p>after</p>");
  });
});

describe("SystemMessage channel refs", () => {
  // Construct a minimal MessageDto. Only the fields SystemMessage /
  // its inner cards read need realistic values; the rest are filled
  // with type-correct stubs.
  function makeSystemMessage(
    subtype: string,
    metadata: Record<string, unknown>,
    content = "",
  ): import("@silverhand/shared").MessageDto {
    return {
      id: "sys-1",
      conversation_id: "conv-1",
      from_kind: "system",
      from_agent_id: null,
      from_user_id: null,
      author: null,
      content,
      metadata: { subtype, ...metadata },
      attachments: null,
      seq: decodeMessageSeq(1),
      created_at: 0,
      thread_root_id: null,
      linked_task: null,
      reactions: null,
    };
  }

  function renderSystem(
    msg: import("@silverhand/shared").MessageDto,
    channelsByName: Map<string, ChannelReference | null>,
    conversationRefsById?: Map<string, WorkspaceChannelReference>,
    humansByName?: Map<string, UserDto>,
  ): string {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    return renderToStaticMarkup(
      React.createElement(
        StaticRouter,
        { location: "/" },
        React.createElement(
          QueryClientProvider,
          { client: qc },
          React.createElement(SystemMessage, {
            message: msg,
            channelsByName,
            conversationRefsById,
            humansByName,
            onOpenConversationRef: () => undefined,
            onOpenChannelRef: () => undefined,
          }),
        ),
      ),
    );
  }

  it("renders retired thread_summary through generic narration content", () => {
    // thread_summary is no longer a live subtype with a structured card.
    // If an unexpected old row appears, it falls through to generic
    // narration and renders message.content, not metadata.summary.
    const channels = new Map<string, ChannelReference | null>([
      ["version3", { id: "ch-v3", title: "version3" }],
    ]);
    const msg = makeSystemMessage(
      "thread_summary",
      { thread_title: "t1", summary: "ignored #missing" },
      "see #version3 for context",
    );
    const html = renderSystem(msg, channels);
    expect(html).toContain('data-channel-ref-state="resolved"');
    expect(html).toContain('data-channel-ref-id="ch-v3"');
    expect(html).not.toContain("ignored");
  });

  it("resolves #<channel_name> inside a non-collapsible system narration", () => {
    // join/leave / generic system messages render `message.content`
    // through Markdown directly. Use an unknown subtype to land in the
    // !collapsible branch.
    const channels = new Map<string, ChannelReference | null>([
      ["version3", { id: "ch-v3", title: "version3" }],
    ]);
    const msg = makeSystemMessage("unknown", {}, "broadcast — #version3 archived");
    const html = renderSystem(msg, channels);
    expect(html).toContain('data-channel-ref-state="resolved"');
    expect(html).toContain('data-channel-ref-id="ch-v3"');
  });

  it("resolves @human handles inside markdown-backed system bodies", () => {
    const humans = new Map<string, UserDto>([
      [
        "shiki",
        {
          id: "user-shiki",
          name: "shiki",
          display_name: "Shiki",
          avatar_url: null,
          created_at: 1,
          updated_at: 1,
        },
      ],
    ]);
    const msg = makeSystemMessage(
      "thread_summary",
      { thread_title: "Follow-up", summary: "ignored" },
      "cc @shiki for review.",
    );

    const html = renderSystem(msg, new Map(), undefined, humans);

    expect(html).toContain('data-mention-kind="human"');
    expect(html).toContain('data-mention-id="user-shiki"');
    expect(html).toContain("@Shiki");
  });

  it("renders session_cleared full metadata as inline audit row (no toast styling)", () => {
    const msg = makeSystemMessage(
      "session_cleared",
      {
        agent_id: "a-alice",
        agent_name: "Alice",
        actor_user_id: "local",
        previous_cli_session_id_prefix: "fbe21c8a",
        was_running: true,
      },
      "@local cleared @alice's CLI session context",
    );
    const html = renderSystem(msg, new Map());
    expect(html).toContain('data-session-cleared="1"');
    // Actor + agent identity. Names are
    // bare recognition labels, no leading `@` (#task:1768).
    expect(html).toContain(">you</span>");
    expect(html).toContain(">Alice</span>");
  });

  it("session_cleared with actor_user_id=null marks the actor unknown and must NOT render @you", () => {
    // Schema docstring (`packages/shared/src/system-messages.ts`):
    // null = principal info wasn't recorded at insert time. This is the
    // OPPOSITE of the v1 message-row convention where null = current
    // viewer. Renderer must NOT route null through
    // `formatUserDisplayName` (which would produce `@you`) here.
    const msg = makeSystemMessage("session_cleared", {
      agent_id: "a-bob",
      agent_name: "Bob",
      actor_user_id: null,
      previous_cli_session_id_prefix: "aabbccdd",
      was_running: true,
    });
    const html = renderSystem(msg, new Map());
    expect(html).toContain('data-session-cleared="1"');
    expect(html).toContain('data-session-cleared-actor="unknown"');
    expect(html).not.toContain(">you</span>");
  });

  it("session_cleared with no metadata falls back to legacy audit row + RotateCcw", () => {
    const msg = makeSystemMessage(
      "session_cleared",
      {},
      "@local cleared @alice's CLI session context (prev fbe21c8a)",
    );
    const html = renderSystem(msg, new Map());
    expect(html).toContain('data-session-cleared-legacy="1"');
    expect(html).toContain("CLI session context");
  });

  // PR-5: thread_anchor / child_context / channel_context unified
  // context-card series. Each renders with a left-accent border per
  // kind, an uppercase kind tag, a source chip, and a body that's
  // collapsible (default-expanded only for thread_anchor — the quote
  // body IS the point of the row).

  it("renders thread_anchor as a Quoted card with default-expanded blockquote", () => {
    const msg = makeSystemMessage("thread_anchor", {
      quoted_message_id: "msg-orig",
      quoted_conversation_id: "conv-parent",
      quoted_channel_name: "version3",
      quoted_content: "我们的 system message 渲染太朴素了，能不能优化下 UI？",
      quoted_from_kind: "user",
      quoted_from_agent_id: null,
      quoted_from_user_id: "local",
      quoted_agent_name: null,
      quoted_created_at: Date.now() - 2 * 60_000,
    });
    const refs = new Map<string, WorkspaceChannelReference>([
      ["conv-parent", { id: "conv-parent", title: "version3" }],
    ]);
    const html = renderSystem(msg, new Map(), refs);
    expect(html).toContain('data-context-card="quoted"');
    // Source chip is a clickable ConversationRef pointing at the
    // parent conversation; the prop fast path resolves to the channel
    // title so the chip shows "version3" (the leading # is rendered
    // as a separate Hash icon).
    expect(html).toContain('data-conversation-ref="conv-parent"');
    expect(html).toContain(">version3<");
    // Default expanded → blockquote rendered, no toggle.
    expect(html).toContain("<blockquote");
    expect(html).toContain("我们的 system message");
    expect(html).not.toContain('data-context-toggle="expand"');
  });

  it("renders child_context as an Inherited card collapsed by default", () => {
    const msg = makeSystemMessage("child_context", {
      parent_conversation_id: "conv-parent",
      parent_title: "feature/cli design",
      summary:
        "PR-1 design discussed in 5 rounds; PR-A registry + drift guard is structural prelude.",
    });
    const refs = new Map<string, WorkspaceChannelReference>([
      ["conv-parent", { id: "conv-parent", title: "feature/cli design" }],
    ]);
    const html = renderSystem(msg, new Map(), refs);
    expect(html).toContain('data-context-card="inherited"');
    // Source chip is a clickable ConversationRef resolving to the parent title.
    expect(html).toContain('data-conversation-ref="conv-parent"');
    expect(html).toContain(">feature/cli design<");
    // Default collapsed → expand toggle present.
    expect(html).toContain('data-context-toggle="expand"');
    expect(html).not.toContain("<blockquote");
  });

  it("renders channel_context as a Spawned-from card with clickable source chip", () => {
    const msg = makeSystemMessage(
      "channel_context",
      {
        source_conversation_id: "conv-source",
        source_conversation_title: "version3",
        created_by_agent_id: "a-cindy",
        created_by_agent_name: "Cindy",
      },
      "Continue the system-message redesign discussion in a focused channel.",
    );
    const refs = new Map<string, WorkspaceChannelReference>([
      ["conv-source", { id: "conv-source", title: "version3" }],
    ]);
    const html = renderSystem(msg, new Map(), refs);
    expect(html).toContain('data-context-card="spawned"');
    expect(html).toContain('data-conversation-ref="conv-source"');
    expect(html).toContain(">version3<");
    // Creator agent name is passthrough data, a bare label with no
    // leading `@` (#task:1768).
    expect(html).toContain("by Cindy");
    expect(html).toContain('data-context-toggle="expand"');
  });

  it("channel_context falls back gracefully when source title / agent_name are absent", () => {
    // Pre-PR-5 producer rows only have id pairs (no snapshot title /
    // agent_name). The renderer still keeps the spawned card chrome:
    // ConversationRef itself handles title resolution via
    // useConversationById — under SSR with no map and no resolved
    // hook, it falls back to the `#conv:<short>` short label, so the
    // chip stays clickable. The agent suffix degrades to "by an agent".
    const msg = makeSystemMessage(
      "channel_context",
      {
        source_conversation_id: "conv-source",
        created_by_agent_id: "a-cindy",
      },
      "seed body",
    );
    const html = renderSystem(msg, new Map());
    expect(html).toContain('data-context-card="spawned"');
    // Chip is still present + clickable, just shows the short id.
    expect(html).toContain("#conv:conv-source");
  });

  it("thread_anchor without enriched metadata falls back to legacy muted row", () => {
    const msg = makeSystemMessage(
      "thread_anchor",
      {},
      "Quoted from #version3 · @shiki: legacy quote body text.",
    );
    const html = renderSystem(msg, new Map());
    expect(html).toContain('data-context-card-legacy="1"');
    expect(html).toContain("legacy quote body text");
    // No structured chip / blockquote.
    expect(html).not.toContain("<blockquote");
  });

  it("child_context without summary metadata falls back to legacy muted row", () => {
    const msg = makeSystemMessage(
      "child_context",
      {},
      "Inherited context from parent channel 'feature/cli design': legacy summary",
    );
    const html = renderSystem(msg, new Map());
    expect(html).toContain('data-context-card-legacy="1"');
    expect(html).toContain("legacy summary");
  });

  // PR-1 removed content-prefix inference. Deleted retired subtypes
  // (`join`, `session_context`, `child_summary`) are now unknown strings:
  // they fall through to generic narration and do not pretend to be a
  // current subtype's custom card.
  it("renders retired `child_summary` rows as generic narration, not collapsible", () => {
    const msg = makeSystemMessage("child_summary", {}, "Sub-session 8c2a closed; transcript saved");
    const html = renderSystem(msg, new Map());
    // The collapsible component renders a `<button>` with `aria-expanded`;
    // generic narration renders a centered italic `<div>` instead.
    expect(html).not.toContain("aria-expanded");
    expect(html).toContain("Sub-session 8c2a closed");
  });

  it("renders retired `join` rows as generic narration", () => {
    const msg = makeSystemMessage("join", {}, "@old-agent joined #version3");
    const html = renderSystem(msg, new Map());
    expect(html).not.toContain("aria-expanded");
    expect(html).toContain("@old-agent");
    expect(html).toContain("joined");
  });

  // PR-1 dropped the `inferSubtype` content-prefix backfill. Old rows
  // whose metadata never carried `subtype` no longer get retroactively
  // mapped to `onboarding` based on their content prefix — they
  // resolve to `unknown` and render as generic narration instead.
  it("does NOT collapse pre-metadata onboarding rows by content prefix", () => {
    const msg = makeSystemMessage(
      // No subtype field at all in metadata.
      // @ts-expect-error: deliberately testing the missing-subtype path.
      undefined,
      {},
      "Onboarding task (system-triggered): you joined #foo. Send greeting.",
    );
    // Strip the test helper's auto-injected `subtype` since we want the
    // raw metadata to NOT have one. makeSystemMessage spreads {subtype, ...}
    // so we override here.
    msg.metadata = {};
    const html = renderSystem(msg, new Map());
    expect(html).not.toContain("aria-expanded");
  });

  // PR-6: onboarding gets an explicit "Onboarding" badge alongside
  // the chevron + first-line preview. Same collapsible affordance as
  // PR-1's `CollapsibleNarration`, but the row is now recognisable
  // as an onboarding instruction at a glance.

  it("renders onboarding with badge + chevron + first-line preview (collapsed)", () => {
    const msg = makeSystemMessage(
      "onboarding",
      {},
      "Onboarding task: you (@Alice) joined #version3.\n1) Send greeting.\n2) Stop.",
    );
    const html = renderSystem(msg, new Map());
    expect(html).toContain('data-onboarding-row="1"');
    expect(html).toContain('data-onboarding-badge="1"');
    // First-line preview only when collapsed (default state in SSR).
    expect(html).toContain("Onboarding task: you (@Alice) joined #version3.");
    expect(html).not.toContain("Send greeting");
    expect(html).toContain('aria-expanded="false"');
  });

  // PR-6: rename reads `{old_name, new_name}` and draws a compact
  // audit row (`✎ workspace renamed old → new`) instead of relying
  // on the prose body. Legacy rows without metadata fall through to
  // generic italic narration. The names render as plain inline text —
  // the earlier `<code>` chips read as visual noise (@shiki
  // `otnb0ykIuDaNTtoC`).

  it("renders rename as a data-rename-row audit row with old/new name passthrough when metadata is present", () => {
    // A workspace name is a free-text LABEL, not `#`-addressable (#task:1827):
    // a Chinese name with a space is legal, and the row must render it bare —
    // a `#` prefix would spell a channel reference that resolves to nothing.
    const msg = makeSystemMessage(
      "rename",
      { old_name: "设计 评审", new_name: "version3" },
      "This workspace was renamed from `设计 评审` to `version3`.",
    );
    const html = renderSystem(msg, new Map());
    expect(html).toContain('data-rename-row="1"');
    expect(html).toContain("设计 评审");
    expect(html).toContain("version3");
    expect(html).not.toContain("#设计");
    expect(html).not.toContain("#version3");
  });

  it("renames without enriched metadata fall back to generic italic narration", () => {
    const msg = makeSystemMessage(
      "rename",
      {},
      "This workspace was renamed from `legacy-old` to `legacy-new`.",
    );
    const html = renderSystem(msg, new Map());
    expect(html).not.toContain('data-rename-row="1"');
    // Generic narration is centered italic muted text.
    expect(html).toContain("legacy-old");
    expect(html).toContain("legacy-new");
  });

  it('PR-10 follow-up #7: info-block subtypes get `data-system-layout="info"`; cluster narration does not', () => {
    // Drift guard for the avatar-column / content-column split in
    // `system-message.tsx`'s `INFO_BLOCK_SUBTYPES` set, paired with
    // `.system-message-row[data-system-layout="info"]` in
    // chrome.css. Banners + cards (schedule_fire, context cards,
    // mention_cc, onboarding) must carry the
    // attribute so the avatar-column alignment kicks in; cluster
    // narration (task_* / session_cleared / rename) must NOT carry
    // it because it stays in the regular content column. Adding a
    // new SystemSubtype without a deliberate decision means it
    // falls through to the default content column — that's
    // intentional, the author has to opt the new subtype into the
    // info layout explicitly.
    const infoSubtypes: Array<[string, Record<string, unknown>]> = [
      ["channel_context", { source_conversation_title: "foo", created_by_agent_name: "Alice" }],
      ["child_context", { parent_conversation_id: "p1", parent_title: "parent", summary: "s" }],
      [
        "thread_anchor",
        {
          quoted_message_id: "m1",
          quoted_from_kind: "agent",
          quoted_from_agent_id: "a1",
          quoted_from_agent_name: "Alice",
          quoted_content_excerpt: "hi",
        },
      ],
      ["onboarding", {}],
      [
        "mention_cc",
        { source_conversation_id: "c1", source_user_id: null, source_user_name: "me" },
      ],
    ];
    for (const [subtype, metadata] of infoSubtypes) {
      const msg = makeSystemMessage(subtype, metadata);
      const html = renderSystem(msg, new Map());
      expect(html, `info subtype "${subtype}" should carry data-system-layout="info"`).toContain(
        'data-system-layout="info"',
      );
    }

    // Cluster narration subtypes (rendered as standalone here for
    // the test — grouping is a separate concern) should NOT carry
    // the layout attribute.
    const clusterSubtypes: Array<[string, Record<string, unknown>]> = [
      [
        "task_create",
        {
          task_number: 1,
          task_status: "todo",
          task_id: "task-1",
          task_message_id: "mt",
          task_title: "t",
          task_assignee: null,
          actor: { kind: "agent", id: "a1", name: "Alice" },
        },
      ],
      [
        "session_cleared",
        {
          agent_id: "a1",
          agent_name: "Alice",
          actor_user_id: "u",
          previous_cli_session_id_prefix: "abc",
          was_running: false,
        },
      ],
      ["rename", { old_name: "a", new_name: "b" }],
    ];
    for (const [subtype, metadata] of clusterSubtypes) {
      const msg = makeSystemMessage(subtype, metadata);
      const html = renderSystem(msg, new Map());
      expect(
        html,
        `cluster subtype "${subtype}" should NOT carry data-system-layout`,
      ).not.toContain("data-system-layout=");
    }
  });
});

describe("SystemMessage task event rows (PR-2)", () => {
  function makeSystemMessage(
    subtype: string,
    metadata: Record<string, unknown>,
    content = "",
  ): import("@silverhand/shared").MessageDto {
    return {
      id: "sys-task-1",
      conversation_id: "conv-1",
      from_kind: "system",
      from_agent_id: null,
      from_user_id: null,
      author: null,
      content,
      metadata: { subtype, ...metadata },
      attachments: null,
      seq: decodeMessageSeq(1),
      created_at: 0,
      thread_root_id: null,
      linked_task: null,
      reactions: null,
    };
  }

  function renderTaskEvent(
    msg: import("@silverhand/shared").MessageDto,
    onOpenTask?: (taskId: string) => void,
    seededTasks: TaskSummaryDto[] = [],
  ): string {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    // Pre-seed the React Query cache so the embedded `<TaskRef>` chip
    // resolves to its clickable / inline-title state instead of staying
    // in the SSR `loading` branch. PR-7 collapsed the standalone
    // TaskEventChip onto TaskRef — title rendering is now driven by
    // TaskRef's own `useTaskByNumber` query, so tests must seed it.
    for (const t of seededTasks) {
      if (t.place.kind !== "workspace") throw new Error("workspace fixture expected");
      qc.setQueryData(
        taskByNumberKey(
          taskBoardCacheScope({ workspaceId: t.place.workspace_id, conversationId: "conv-1" }),
          t.task_number,
        ),
        t,
      );
    }
    return renderToStaticMarkup(
      React.createElement(
        StaticRouter,
        { location: "/" },
        React.createElement(
          QueryClientProvider,
          { client: qc },
          React.createElement(
            TaskRefContextProvider,
            { value: { workspaceId: "ws-1", conversationId: "conv-1", onOpenTask } },
            React.createElement(SystemMessage, {
              message: msg,
              channelsByName: new Map(),
              onOpenChannelRef: () => undefined,
            }),
          ),
        ),
      ),
    );
  }

  function seededTask(overrides: Partial<TaskSummaryDto> = {}): TaskSummaryDto {
    return {
      id: "task-42",
      task_number: 42,
      place: {
        kind: "workspace",
        conversation_id: "chan-1",
        workspace_id: "ws-1",
        channel_name: "general",
      },
      thread_root_id: null,
      status: "todo",
      title: "wire registry",
      assignee: null,
      creator: { type: "agent", id: "a-alice", name: "Alice" },
      created_at: 0,
      closed_at: null,
      ...overrides,
    };
  }

  const fullMeta = (overrides: Record<string, unknown>): Record<string, unknown> => ({
    task_number: 42,
    task_status: "todo",
    task_id: "task-42",
    task_message_id: "msg-task-42",
    task_title: "wire registry",
    task_assignee: null,
    actor: { kind: "agent", id: "a-alice", name: "Alice" },
    ...overrides,
  });

  it("task_create renders actor + verb + chip with title + todo dot", () => {
    const msg = makeSystemMessage("task_create", fullMeta({}));
    const html = renderTaskEvent(msg, () => undefined, [seededTask()]);
    expect(html).toContain(">Alice</span>");
    expect(html).toContain('data-task-event-dot="todo"');
    // PR-7: chip is now TaskRef. Resolved+handler state → label
    // `#42 · wire registry`, attribute `data-task-ref-state="resolved"`.
    expect(html).toContain('data-task-ref="42"');
    expect(html).toContain('data-task-ref-state="resolved"');
    expect(html).toContain("wire registry");
  });

  it("task_claim renders verb=claimed + chip + in_progress dot", () => {
    const msg = makeSystemMessage(
      "task_claim",
      fullMeta({
        task_status: "in_progress",
        task_assignee: { type: "agent", id: "a-alice", name: "Alice" },
      }),
    );
    const html = renderTaskEvent(msg, () => undefined, [seededTask({ status: "in_progress" })]);
    expect(html).toContain('data-task-event-dot="in_progress"');
    expect(html).toContain('data-task-ref="42"');
  });

  it("task_unclaim uses dashed-border dot (no fill)", () => {
    const msg = makeSystemMessage(
      "task_unclaim",
      fullMeta({ task_status: "todo", task_assignee: null }),
    );
    const html = renderTaskEvent(msg, () => undefined, [seededTask()]);
    expect(html).toContain('data-task-event-dot="unclaim"');
    expect(html).toContain('data-task-ref="42"');
  });

  it("task_status renders actor moved chip → status pill (PR-9 actor-first)", () => {
    const msg = makeSystemMessage("task_status", fullMeta({ task_status: "in_review" }));
    const html = renderTaskEvent(msg, () => undefined, [seededTask({ status: "in_review" })]);
    expect(html).toContain(">Alice</span>");
    expect(html).toContain('data-task-ref="42"');
    expect(html).toContain('data-task-event-status="in_review"');
    // PR-9: actor is now the first text node, NOT a "by <actor>"
    // suffix. Verify we don't accidentally re-introduce the old form.
    expect(html).not.toContain("by Alice");
    // Reading order: actor → task-ref chip → status marker. Keyed on
    // stable nodes, not the connecting verb (which is copy).
    expect(html.indexOf(">Alice</span>")).toBeLessThan(html.indexOf('data-task-ref="42"'));
    expect(html.indexOf('data-task-ref="42"')).toBeLessThan(
      html.indexOf('data-task-event-status="in_review"'),
    );
  });

  it('user actor renders the viewer-friendly "you" label for current user', () => {
    const msg = makeSystemMessage(
      "task_status",
      fullMeta({
        task_status: "done",
        // Multi-human audit 2026-06-03 #task:857 P0.5 (legacy null-
        // actor → "you" collapse was removed). To exercise the
        // viewer-rendering path now, the actor id must match the
        // configured viewer (`'local'`, set above in this file).
        actor: { kind: "user", id: "local", name: "user" },
      }),
    );
    const html = renderTaskEvent(msg, () => undefined, [seededTask({ status: "done" })]);
    expect(html).toContain('data-task-event-status="done"');
    // PR-9 actor-first: `you moved ... → DONE`, not `... by you`. The
    // viewer's own id renders the friendly bare `you` label (#task:1768).
    expect(html).toContain(">you</span>");
    expect(html).not.toContain("by you");
  });

  it("chip is inert (no click handler) when no onOpenTask handler is supplied", () => {
    const msg = makeSystemMessage("task_create", fullMeta({}));
    const html = renderTaskEvent(msg, undefined, [seededTask()]);
    // PR-7: TaskRef now drives the chip. Without a handler in
    // TaskRefContext, the resolved chip falls into the
    // `resolved-no-handler` branch — no `role="button"`.
    expect(html).toContain('data-task-ref="42"');
    expect(html).toContain('data-task-ref-state="resolved-no-handler"');
    expect(html).not.toContain('data-task-ref-state="resolved"');
  });

  it("legacy row with partial metadata (subtype + task_number + task_status) renders degraded chip + pill", () => {
    // Pre-PR-2 task system message — only subtype + task_number + task_status
    // in metadata. Renderer must NOT throw on Zod (full) failure; mid-tier
    // degraded fallback surfaces a chip + status pill so the audit entry
    // is still self-identifying without re-parsing content.
    const msg = makeSystemMessage(
      "task_create",
      { task_number: 9, task_status: "todo" },
      '@alice created task #9 "old format"',
    );
    const html = renderTaskEvent(msg, () => undefined);
    expect(html).toContain('data-task-event-degraded="1"');
    expect(html).toContain('data-task-event-dot="todo"');
    expect(html).toContain("#task:9");
    // Title not in legacy metadata → chip has no inline title.
    expect(html).not.toContain("old format");
    expect(html).not.toContain("data-task-event-legacy");
  });

  it("legacy task_status row renders chip → pill in degraded form (no by-actor)", () => {
    const msg = makeSystemMessage("task_status", {
      task_number: 12,
      task_status: "in_review",
    });
    const html = renderTaskEvent(msg, () => undefined);
    expect(html).toContain('data-task-event-degraded="1"');
    expect(html).toContain('data-task-event-status="in_review"');
    expect(html).toContain("#task:12");
    expect(html).not.toContain("by @");
  });

  it("legacy task_unclaim still uses dashed dot in degraded form", () => {
    const msg = makeSystemMessage("task_unclaim", {
      task_number: 4,
      task_status: "todo",
    });
    const html = renderTaskEvent(msg, () => undefined);
    expect(html).toContain('data-task-event-degraded="1"');
    expect(html).toContain('data-task-event-dot="unclaim"');
  });

  it("total fallback (metadata missing even task_number) → muted narration", () => {
    // Belt+suspenders: metadata so malformed even the partial schema
    // can't parse it. Renderer drops to italic-content narration.
    const msg = makeSystemMessage(
      "task_create",
      {
        // Force a schema mismatch — task_status omitted entirely.
      },
      "broken task row content",
    );
    const html = renderTaskEvent(msg, () => undefined);
    expect(html).toContain('data-task-event-legacy="1"');
    expect(html).toContain("broken task row content");
    expect(html).not.toContain("data-task-event-degraded");
  });

  it("PR-8: every system message renders inside the system event-row wrapper", () => {
    // Drift guard: the SystemMessage entry wraps every per-subtype renderer
    // in the shared system event-row container (marked
    // `data-system-event-row="1"`), which chrome.css hooks the theme prose
    // font/size cascade onto. Removing the wrapper would silently strip
    // system rows out of that cascade. Keyed on the data marker, not the
    // class name (the class string is a styling detail).
    const enriched = makeSystemMessage("task_create", fullMeta({}));
    const enrichedHtml = renderTaskEvent(enriched, () => undefined, [seededTask()]);
    expect(enrichedHtml).toMatch(/^<div[^>]*\bdata-system-event-row="1"/);

    const legacy = makeSystemMessage(
      "task_create",
      { task_number: 9, task_status: "todo" },
      "@alice created task #9",
    );
    const legacyHtml = renderTaskEvent(legacy, () => undefined);
    expect(legacyHtml).toMatch(/^<div[^>]*\bdata-system-event-row="1"/);
  });
});

describe("Markdown channel refs", () => {
  function renderWithChannels(
    content: string,
    channelsByName: Map<string, ChannelReference | null>,
    onOpenChannelRef?: (channelId: string) => void,
  ): string {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    return renderToStaticMarkup(
      React.createElement(
        QueryClientProvider,
        { client: qc },
        React.createElement(Markdown, {
          content,
          channelsByName,
          onOpenChannelRef,
        }),
      ),
    );
  }

  it("renders #<channel_name> as a clickable accent pill when uniquely resolved", () => {
    const channels = new Map<string, ChannelReference | null>([
      ["version3", { id: "ch-v3-id", title: "version3" }],
    ]);
    const html = renderWithChannels("See #version3 for context.", channels, () => undefined);
    expect(html).toContain('data-channel-ref-state="resolved"');
    expect(html).toContain('data-channel-ref-id="ch-v3-id"');
    // The chip displays the channel title; the bare `#version3` literal
    // shouldn't survive (since the chip prefixes the icon, not text).
    expect(html).toContain("version3");
    expect(html).not.toContain(">#version3<");
  });

  it("renders ambiguous #<channel_name> as raw prose", () => {
    // Sentinel `null` value = two channels in the workspace share this
    // title; the renderer must not silently pick one.
    const channels = new Map<string, ChannelReference | null>([["general", null]]);
    const html = renderWithChannels("See #general here.", channels, () => undefined);
    expect(html).not.toContain("data-channel-ref-state=");
    expect(html).toContain("#general");
    expect(html).not.toContain("data-channel-ref-id");
  });

  it("renders unresolved #<channel_name> as raw prose", () => {
    // Empty map → no entry for `version3` → unresolved branch.
    const html = renderWithChannels("See #unknown here.", new Map(), () => undefined);
    expect(html).not.toContain("data-channel-ref-state=");
    expect(html).toContain("#unknown");
    expect(html).not.toContain("data-channel-ref-id");
  });

  it("still renders inert (no role=button) when click handler is omitted", () => {
    const channels = new Map<string, ChannelReference | null>([
      ["version3", { id: "ch-v3-id", title: "version3" }],
    ]);
    const html = renderWithChannels("See #version3.", channels);
    expect(html).toContain('data-channel-ref-state="resolved-no-handler"');
    expect(html).not.toContain('role="button"');
  });
});

describe("Markdown human mentions", () => {
  it("renders resolved human @handle as a human mention chip", () => {
    const humans = new Map<string, UserDto>([
      [
        "shiki",
        {
          id: "user-shiki",
          name: "shiki",
          display_name: "Shiki",
          avatar_url: null,
          created_at: 1,
          updated_at: 1,
        },
      ],
    ]);

    const html = renderWithHumans("cc @shiki on this.", humans);

    expect(html).toContain('data-mention-kind="human"');
    expect(html).toContain('data-mention-id="user-shiki"');
    expect(html).toContain("@Shiki");
    expect(html).not.toContain(">@shiki<");
  });

  it("keeps unresolved @handle as muted text", () => {
    const html = renderWithHumans("cc @missing on this.", new Map());

    expect(html).toContain("@missing");
    expect(html).not.toContain('data-mention-kind="human"');
  });

  it("does not parse @handle after a path slash", () => {
    const humans = new Map<string, UserDto>([
      [
        "shiki",
        {
          id: "user-shiki",
          name: "shiki",
          display_name: "Shiki",
          avatar_url: null,
          created_at: 1,
          updated_at: 1,
        },
      ],
    ]);

    const html = renderWithHumans("Literal path/@shiki stays text.", humans);

    expect(html).toContain("path/@shiki");
    expect(html).not.toContain('data-mention-kind="human"');
    expect(html).not.toContain('data-mention-id="user-shiki"');
  });
});

/**
 * #task:2001 — the canonical reference arm. A mechanical fence, not a painting
 * test: the `a` override's LAST arm renders a real `<a>` whose click handler
 * calls `window.silverhand.openExternal(href)`, so it is the only arm that can
 * produce an `<a>` element. Asserting no `<a>` for a rejected destination
 * therefore proves the destination never reached the OS opener — the one failure
 * this design must not have, and one that would otherwise only show up as a
 * browser window opening on a user's machine.
 *
 * Every case runs in BOTH branches. `interactive: false` is the export /
 * screenshot path with its own independent copy of the arm chain, and a fence
 * that only covered the live path would let the export path regress silently.
 *
 * These render a real body rather than calling the arm with an href, because
 * under ruling C (shiki, 2026-08-13) the SPELLING is what decides: `remarkRefs`
 * stamps provenance on bare autolinks and only stamped destinations chip.
 * `reference-href.test.ts` covers the arm's own table; the stamping half, and the
 * fact that a body cannot forge the stamp, can only be observed from source.
 */
describe("Markdown canonical references (#task:2000)", () => {
  const REF_AGENT: AgentDto = {
    id: "agent-alice",
    name: "Alice",
    display_name: "Alice L",
    runtime: "claude",
    avatar_url: null,
    created_at: 1,
    updated_at: 1,
  } as AgentDto;

  const REF_HUMAN: UserDto = {
    id: "user-shiki",
    name: "shiki",
    display_name: "Shiki",
    avatar_url: null,
    created_at: 1,
    updated_at: 1,
  };

  function renderRef(content: string, interactive: boolean): string {
    const qc = new QueryClient({
      defaultOptions: { queries: { retry: false } },
    });
    return renderToStaticMarkup(
      React.createElement(
        QueryClientProvider,
        { client: qc },
        React.createElement(
          StaticRouter,
          { location: "/" },
          React.createElement(Markdown, {
            content,
            interactive,
            agentsByName: new Map([[REF_AGENT.name, REF_AGENT]]),
            humansByName: new Map([[REF_HUMAN.name, REF_HUMAN]]),
          }),
        ),
      ),
    );
  }

  for (const interactive of [true, false]) {
    const branch = interactive ? "live" : "export";

    it(`resolves an agent reference autolink to an agent chip (${branch})`, () => {
      const html = renderRef("cc <silverhand://agent/Alice> please", interactive);

      expect(html).toContain('data-mention-kind="agent"');
      expect(html).toContain("Alice L");
      // The destination is consumed, never emitted.
      expect(html).not.toContain("silverhand://");
      expect(html).not.toContain("<a ");
    });

    it(`resolves a human reference autolink to a human chip (${branch})`, () => {
      const html = renderRef("cc <silverhand://human/shiki> please", interactive);

      expect(html).toContain('data-mention-kind="human"');
      expect(html).toContain('data-mention-id="user-shiki"');
      expect(html).not.toContain("silverhand://");
      expect(html).not.toContain("<a ");
    });

    it(`does not resolve a reference across kinds (${branch})`, () => {
      // `Alice` is an agent here. An `human/Alice` reference names a human by
      // that name, and there is none — so it stays an unresolved chip and must
      // not borrow the agent's identity.
      const html = renderRef("cc <silverhand://human/Alice>", interactive);

      expect(html).not.toContain('data-mention-id="agent-alice"');
      expect(html).not.toContain("silverhand://");
      expect(html).not.toContain("<a ");
    });

    it(`renders a reference to a non-member as an unresolved chip (${branch})`, () => {
      const html = renderRef("cc <silverhand://agent/Nobody>", interactive);

      expect(html).toContain("Nobody");
      expect(html).not.toContain("silverhand://");
      expect(html).not.toContain("<a ");
    });

    it(`keeps every malformed destination unclickable (${branch})`, () => {
      // Each of these is our scheme (so the sanitizer let it through) but not a
      // reference (so the strict parser refuses it). The arm must show the
      // authored text and stop there.
      for (const href of [
        "silverhand://",
        "silverhand://agent",
        "silverhand://agent/",
        "silverhand://team/Alice",
        "silverhand://agent/Alice/extra",
        "silverhand://agent/Alice?x=1",
        "silverhand://app",
        "SILVERHAND://agent/Alice",
      ]) {
        const html = renderRef(`see <${href}> here`, interactive);
        expect(html, href).not.toContain("<a ");
        expect(html, href).not.toContain("data-mention-kind=");
      }
    });

    it(`ignores a label-carrying link to the reference scheme (${branch})`, () => {
      // The addressing side refuses these because the label can name a different
      // identity than the destination. Display refuses them too, for a reason of
      // its own: the chip REPLACES the label, so chipping this would drop "Alice"
      // from the rendered message while the raw body — CLI history, export, agent
      // stdin — still shows it. Same body, two readings.
      const html = renderRef("[Alice](silverhand://human/shiki) wrote it", interactive);

      expect(html).toContain("Alice");
      expect(html).not.toContain("data-mention-kind=");
      expect(html).not.toContain("silverhand://");
      expect(html).not.toContain("<a ");
    });

    it(`does not chip a labelled link even when the label IS its destination (${branch})`, () => {
      // Ruling C (shiki, 2026-08-13). Display admits exactly what the addressing
      // side admits — bare autolinks — so this spelling, which yields no
      // reference, must not look addressed. It renders as its own authored text
      // and nothing else.
      const html = renderRef("[silverhand://agent/Alice](silverhand://agent/Alice)", interactive);

      expect(html).not.toContain("data-mention-kind=");
      // The authored text survives, and stays unclickable: an internal scheme
      // must never reach `openExternal`.
      expect(html).toContain("silverhand://agent/Alice");
      expect(html).not.toContain("<a ");
    });

    it(`chips a reference inside a blockquote, which the extractor refuses (${branch})`, () => {
      // The two axes share a SYNTAX test, not a position policy: quoting a
      // message must keep looking like the message, so the chip stays; nobody is
      // addressed by it, which the addressing side decides and is not visible
      // here.
      const html = renderRef("> cc <silverhand://agent/Alice>", interactive);

      expect(html).toContain('data-mention-kind="agent"');
      expect(html).not.toContain("<a ");
    });

    it(`does not chip a reference written inside a code span (${branch})`, () => {
      // Documentation and agent prose quote the syntax constantly; a chip there
      // would make the example unreadable.
      const html = renderRef("write `<silverhand://agent/Alice>`", interactive);

      expect(html).toContain("silverhand://agent/Alice");
      expect(html).not.toContain("data-mention-kind=");
      expect(html).not.toContain("<a ");
    });

    it(`chips a reference in a body that begins with a BOM (${branch})`, () => {
      // The DISPLAY half of a shared-authority defect (#task:2172). The Markdown
      // parser drops one leading U+FEFF and reports its offsets against the
      // remainder, while `remarkRefs` slices the document it was handed — so every
      // autolink in a BOM-prefixed body failed the syntax test and nothing was
      // stamped. Silent, and the same predicate decides addressing, so a message
      // pasted out of a file rendered as bare text AND woke nobody.
      //
      // The correction lives in shared's `isAutolink`, which now owns the coordinate
      // adjustment, so this passes with no renderer change: one authority, and the
      // fanout and the chip cannot disagree about a body's bytes again.
      for (const body of [
        "\ufeff<silverhand://agent/Alice>",
        "\ufeffcc <silverhand://agent/Alice> please",
        // Every link in the document was affected, not only one against the prefix.
        "\ufeffline one\n\nline two <silverhand://agent/Alice>",
      ]) {
        const html = renderRef(body, interactive);

        expect(html, body).toContain('data-mention-kind="agent"');
        expect(html, body).toContain("Alice L");
        expect(html, body).not.toContain("silverhand://");
      }
    });

    it(`still refuses a labelled link in a body that begins with a BOM (${branch})`, () => {
      // The prefix is a coordinate origin, not a licence: what the syntax test
      // admits is unchanged, so the spelling that must never look addressed still
      // does not chip, and its authored label just renders.
      const html = renderRef("\ufeff[Alice](silverhand://human/shiki) wrote it", interactive);

      expect(html).toContain("Alice");
      expect(html).not.toContain("data-mention-kind=");
      expect(html).not.toContain("silverhand://");
      expect(html).not.toContain("<a ");
    });
  }

  it("leaves an external link clickable on the live path", () => {
    // The scheme admission added for references must not narrow what the
    // default sanitizer already allowed.
    const html = renderRef("see <https://example.com/x> here", true);

    expect(html).toContain("<a ");
    expect(html).toContain('href="https://example.com/x"');
  });

  it("still strips the href from an external link on the export path", () => {
    const html = renderRef("see <https://example.com/x> here", false);

    expect(html).not.toContain("<a ");
    expect(html).toContain("https://example.com/x");
  });

  it("keeps the plain @handle alias rendering as a chip", () => {
    // Both spellings draw the same chip (shiki, 2026-08-13): the alias is a
    // permanent display affordance and addresses nobody — it yields no canonical
    // reference, so no notification, no follow / CC, no directed framing.
    const html = renderRef("cc @Alice and <silverhand://agent/Alice>", true);

    expect(html.match(/data-mention-kind="agent"/g)).toHaveLength(2);
  });

  it("never leaks the provenance flag into the DOM, on either arm", () => {
    // Provenance is mdast metadata that exists only between the remark pass and
    // the `a` override. Every arm that consumes it renders its own markup, so the
    // flag must not survive into the document — and neither must react-markdown's
    // `node` prop, which the override now destructures instead of spreading onto
    // the plain-link `<a>`.
    //
    // The authored destination (#task:2120) is held to the same fence and needs it
    // MORE: it is a whole URL rather than a boolean, so leaking it would put an
    // internal destination back into the document as an attribute — the one place
    // this design keeps it out of.
    for (const body of [
      "cc <silverhand://agent/Alice>",
      "cc <silverhand://agent/Nobody>",
      "cc <silverhand://team/Alice>",
      "[Alice](silverhand://human/shiki)",
      "cc @Alice and #task:12 and #conv:AbC123_-xyZ789qR",
      "see <https://example.com/x>",
      "in <silverhand://workspace/silverhand>",
      "in <silverhand://workspace/silverhand/UI%2FUX%20优化>",
      "in <silverhand://dm/Cindy/shiki>",
      "[here](silverhand://workspace/silverhand)",
    ]) {
      for (const interactive of [true, false]) {
        const html = renderRef(body, interactive);
        expect(html, body).not.toContain(PLUGIN_LINK_PROP);
        expect(html, body).not.toContain(AUTHORED_HREF_PROP);
        expect(html, body).not.toContain(" node=");
      }
    }
  });

  for (const interactive of [true, false]) {
    const branch = interactive ? "live" : "export";

    it(`refuses a hand-written internal chip scheme (${branch})`, () => {
      // A message body is attacker input, so provenance cannot be a string in the
      // href (@Cindy, #task:2001). Each of these is spelled exactly as the remark
      // pass spells the destinations it produces, and each would draw a chip over
      // text the author wrote — Alice's chip on the word "anything" — if the arm
      // trusted the prefix alone. They render as their own text instead, and
      // never as a link: the plain-link arm hands its href to the OS opener.
      for (const body of [
        "[anything](silverhand-mention:Alice)",
        "<silverhand-mention:Alice>",
        "[Bob](silverhand-task:12)",
        "[Bob](silverhand-conversation:AbC123_-xyZ789qR)",
        "[Bob](silverhand-channel:general)",
      ]) {
        const html = renderRef(body, interactive);
        expect(html, body).not.toContain("data-mention-kind=");
        expect(html, body).not.toContain("<a ");
        // The authored text stays visible (an autolink's text IS its
        // destination), but never as a destination: no arm turns it into an
        // href, so nothing can be handed to the OS opener.
        expect(html, body).not.toContain('href="silverhand');
      }
    });

    it(`refuses a hand-written destination that mimics the old marker (${branch})`, () => {
      // The first implementation of ruling C carried provenance as an internal
      // `silverhand-ref:` scheme wrapped around the destination, which any author
      // could type. It is not a scheme this renderer knows any more, so the
      // sanitizer empties the href and no arm claims it: no chip either way.
      for (const body of [
        "[anything](silverhand-ref:silverhand://agent/Alice)",
        "<silverhand-ref:silverhand://agent/Alice>",
      ]) {
        const html = renderRef(body, interactive);
        expect(html, body).not.toContain("data-mention-kind=");
        expect(html, body).not.toContain('href="silverhand');
      }
    });
  }
});

describe("Markdown conversation refs", () => {
  it("renders #conv:<conversation_id> as a clickable conversation pill", () => {
    const id = "AbC123_-xyZ789qR";
    const refs = new Map<string, WorkspaceChannelReference>([
      [
        id,
        {
          id,
          title: "version3",
        },
      ],
    ]);

    const html = render(`See #conv:${id} for details.`, refs);

    expect(html).toContain(`data-conversation-ref="${id}"`);
    expect(html).toContain("version3");
    expect(html).not.toContain("#version3");
    expect(html).not.toContain(`#conv:${id}`);
  });

  it("preserves unresolved conversation ids as prose", () => {
    const html = render("Open #conv:foreignRef123456.");

    expect(html).toContain("#conv:foreignRef123456");
    expect(html).toContain("#conv:foreignRef123456");
  });

  it("does not parse short refs, longer id-like tokens, code, or existing links", () => {
    const html = render(
      [
        "short #conv:foo",
        "long #conv:foreignRef12345678x",
        "`#conv:codeRef12345678`",
        "[#conv:linkedRef123456](https://example.com)",
        "#conv:realRef123456789.bar",
      ].join(" "),
    );

    expect(html).toContain("#conv:realRef123456789.bar");
    expect(html).not.toContain('data-conversation-ref="foo"');
    expect(html).not.toContain('data-conversation-ref="foreignRef12345678"');
    expect(html).not.toContain('data-conversation-ref="codeRef12345678"');
    expect(html).not.toContain('data-conversation-ref="linkedRef123456"');
    expect(html).toContain(".bar");
  });

  it("does not parse legacy `#~<id>` form (hard cutover, no alias)", () => {
    // Ensure the pre-cutover sigil shape is silently demoted to inert
    // text. Hard cutover per @shiki direction: legacy form had only a
    // few days of usage and no production references depend on it.
    const html = render("See #~AbC123_-xyZ789qR for details.");
    expect(html).not.toContain('data-conversation-ref="AbC123_-xyZ789qR"');
    expect(html).toContain("#~AbC123_-xyZ789qR");
  });

  it("resolves cross-workspace #conv: via useConversationById when prop is missing", () => {
    // Cindy review of commit 3 (`45fca42`): the original implementation
    // only used the synchronous `conversationRefsById` map, which is
    // workspace-scoped — any `#conv:<id>` pointing at a foreign
    // workspace would render permanently as `#conv:<short>` and the
    // popover would skeleton forever. ConversationRef now also calls
    // `useConversationById(id)` so cross-workspace refs resolve. We
    // pre-seed React Query's cache with the resolved payload so SSR
    // observes it as `resolved` without hitting the network.
    const id = "CrossWsRef12345_";
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    qc.setQueryData(["conversation-ref", id], {
      ref: {
        id,
        roster_policy: "open",
        title: "foreign-channel",
        workspace_id: "ws-foreign",
        closed_at: null,
      },
    });
    const html = renderToStaticMarkup(
      React.createElement(
        QueryClientProvider,
        { client: qc },
        React.createElement(Markdown, {
          content: `See #conv:${id}.`,
          // Intentionally omit `conversationRefsById` so the hook is
          // the only resolution path — exercises the cross-workspace
          // case end-to-end.
          onOpenConversationRef: () => undefined,
        }),
      ),
    );
    expect(html).toContain(`data-conversation-ref="${id}"`);
    // Resolved title from the hook surfaces on the chip — proves the
    // hook→prop fallback chain works and the chip didn't get stuck
    // on the `#conv:<short>` placeholder.
    expect(html).toContain("foreign-channel");
    expect(html).not.toContain(`#conv:${id.slice(0, 6)}`);
  });
});

describe("Markdown task refs — pending resolution", () => {
  it("preserves prose while loading", () => {
    const html = renderWithTaskCtx("Working on #task:5 now.", "w-eng", () => {});
    expect(html).toContain("<p>Working on #task:5 now.</p>");
  });

  it("preserves prose without resolution context", () => {
    const html = renderWithTaskCtx("See #task:42.", null);
    expect(html).toContain("<p>See #task:42.</p>");
  });
});

describe("Markdown task refs", () => {
  it("preserves explicit task text while metadata loads", () => {
    const html = renderWithTaskCtx("Working on #task:5 now.", "w-eng", () => {});
    expect(html).toContain("<p>Working on #task:5 now.</p>");
    expect(html).not.toContain("silverhand-channel:task");
  });

  it("parses slash-separated task refs as two live refs", () => {
    const html = renderWithTaskCtx(
      "Compare #task:317/#task:318 before closing.",
      "w-eng",
      () => {},
    );

    expect(html).toContain("#task:317");
    expect(html).toContain("#task:318");
    expect(html.indexOf("#task:317")).toBeLessThan(html.indexOf("#task:318"));
  });

  it("keeps refs after double slash inert", () => {
    const html = renderWithTaskCtx("Compare #task:5//#task:6 before closing.", "w-eng", () => {});

    expect(html).toContain("#task:5");
    expect(html).toContain("#task:6");
  });

  it("parses space-separated refs around slash punctuation", () => {
    const html = renderWithTaskCtx("Compare #task:5 / #task:6 before closing.", "w-eng", () => {});

    expect(html).toContain("#task:5");
    expect(html).toContain("#task:6");
  });

  it("parses slash-separated task and channel refs", () => {
    const html = renderWithTaskCtx(
      "Coordinate #task:5/#general before closing.",
      "w-eng",
      () => {},
    );

    expect(html).toContain("#task:5");
    expect(html).not.toContain("data-channel-ref-state=");
    expect(html).toContain("#general");
  });

  it("renders inert `#task:<n>` text outside any TaskRefContext", () => {
    const html = renderWithTaskCtx("See #task:42.", null);
    expect(html).toContain("#task:42");
    expect(html).not.toContain("data-task-ref=");
  });

  it("does not match malformed task sigils", () => {
    const html = renderWithTaskCtx(
      [
        "see#1", // attached to word — leading-boundary guard blocks
        "path/#task:2", // slash is a path separator unless it follows a live ref
        "#task:abc", // non-numeric value
        "`#task:99`", // inside inline code
        "[#task:7](https://example.com)", // inside existing link
      ].join(" "),
      "w-eng",
      () => {},
    );
    expect(html).not.toContain('data-task-ref=\"1\"');
    expect(html).not.toContain('data-task-ref=\"2\"');
    expect(html).not.toContain('data-task-ref=\"99\"');
    expect(html).not.toContain('data-task-ref=\"7\"');
    expect(html).not.toContain("silverhand-task:abc");
    expect(html).not.toContain("silverhand-channel:task");
  });

  it("rejects #task:0 / leading-zero / mixed-suffix forms (Cindy review of 80f544e)", () => {
    const html = renderWithTaskCtx(
      ["#task:0", "#task:01", "#task:12abc"].join(" "),
      "w-eng",
      () => {},
    );
    expect(html).not.toContain('data-task-ref=\"0\"');
    expect(html).not.toContain('data-task-ref=\"01\"');
    expect(html).not.toContain('data-task-ref=\"12\"');
    expect(html).not.toContain("silverhand-task:0");
    expect(html).not.toContain("silverhand-task:12");
    expect(html).toContain("#task:0");
  });

  it("mixes #task and #conv sigils in the same paragraph", () => {
    const html = renderWithTaskCtx(
      "Forwarded #task:7 from #conv:AbC123_-xyZ789qR — see thread.",
      "w-eng",
      () => {},
    );
    expect(html).toContain("#task:7");
    expect(html).toContain("#conv:AbC123_-xyZ789qR");
    expect(html.indexOf("#task:7")).toBeLessThan(html.indexOf("#conv:AbC123_-xyZ789qR"));
  });
});

describe("Markdown bare task refs (#task:1430)", () => {
  function bareTask(overrides: Partial<TaskSummaryDto> = {}): TaskSummaryDto {
    return {
      id: "task-bare",
      task_number: 42,
      place: {
        kind: "workspace",
        conversation_id: "chan-1",
        workspace_id: "w-eng",
        channel_name: "general",
      },
      thread_root_id: null,
      status: "todo",
      title: "wire registry",
      assignee: null,
      creator: { type: "agent", id: "a-alice", name: "Alice" },
      created_at: 0,
      closed_at: null,
      ...overrides,
    };
  }

  function renderBare(
    content: string,
    // The single board id these bare-task fixtures live on. It is BOTH the
    // scope discriminator (a workspace board keys by `workspace:<id>`) and the
    // conversation the chip is anchored in for the HTTP-lookup gate — the same
    // one value played both roles before B4 keyed the cache by board scope
    // (#task:2041). `null` renders an unresolved/inert chip.
    workspaceId: string | null,
    seeded: TaskSummaryDto[] = [],
    onOpenTask?: (taskId: string) => void,
  ): string {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    for (const t of seeded) {
      if (t.place.kind !== "workspace") throw new Error("workspace fixture expected");
      qc.setQueryData(
        taskByNumberKey(
          taskBoardCacheScope({ workspaceId: t.place.workspace_id, conversationId: null }),
          t.task_number,
        ),
        t,
      );
    }
    return renderToStaticMarkup(
      React.createElement(
        QueryClientProvider,
        { client: qc },
        React.createElement(
          TaskRefContextProvider,
          { value: { workspaceId, conversationId: workspaceId, onOpenTask } },
          React.createElement(Markdown, { content }),
        ),
      ),
    );
  }

  it("renders a resolved bare `#<n>` as a clickable task chip", () => {
    const html = renderBare("Fixing #42 today.", "w-eng", [bareTask()], () => {});
    expect(html).toContain('data-task-ref="42"');
    expect(html).toContain('data-task-ref-state="resolved"');
    // Resolved chip carries `#<n> · title` — same chrome as `#task:<n>`.
    expect(html).toContain("wire registry");
  });

  it("demotes an unresolved bare `#<n>` to plain text (no chip)", () => {
    // Workspace context present but no matching task in cache → SSR sees
    // the query unsettled → bare demotes to its literal text, no chrome.
    // This is the core `#task:1430` contract: a bare number that isn't a
    // task must render exactly as authored.
    const html = renderBare("We shipped #999 improvements.", "w-eng");
    expect(html).not.toContain('data-task-ref="999"');
    // The internal scheme must never leak into rendered HTML.
    expect(html).not.toContain("silverhand-task-bare:999");
    expect(html).toContain("#999");
  });

  it("demotes a bare `#<n>` to plain text with no workspace context", () => {
    const html = renderBare("See #5 here.", null);
    expect(html).not.toContain('data-task-ref="5"');
    expect(html).toContain("#5");
  });

  it("chips only the numbers that resolve to real tasks", () => {
    // Only #42 is seeded; #7 is a random number → chip for 42, plain 7.
    const html = renderBare("Compare #42 and #7.", "w-eng", [bareTask()], () => {});
    expect(html).toContain('data-task-ref="42"');
    expect(html).not.toContain('data-task-ref="7"');
    expect(html).toContain("#7");
  });

  it("does not regress the explicit `#task:<n>` sigil (no bare hijack)", () => {
    const html = renderBare("Working on #task:5 now.", "w-eng", [], () => {});
    expect(html).toContain("<p>Working on #task:5 now.</p>");
  });

  it("does not match bare `#0` / leading-zero forms", () => {
    const html = renderBare("#0 #007 done", "w-eng", [], () => {});
    expect(html).not.toContain('data-task-ref="0"');
    expect(html).not.toContain('data-task-ref="7"');
    expect(html).not.toContain('data-task-ref="007"');
    expect(html).toContain("#0");
  });

  it("does not match `#<n>` attached to a preceding word", () => {
    // Even though task 3 is seeded, the leading-boundary guard blocks a
    // `#3` glued to a word (`issue#3`) so mid-token noise stays prose.
    const html = renderBare(
      "issue#3 stays text",
      "w-eng",
      [bareTask({ task_number: 3 })],
      () => {},
    );
    expect(html).not.toContain('data-task-ref="3"');
    expect(html).toContain("issue#3");
  });

  it("leaves bare `#1` as plain text (list-marker habit `#1, #2, #3`)", () => {
    // Seeded so a resolvable path exists — the numeric-range guard must
    // still keep 1–10 out of the bare tokenizer so numbered lists don't
    // get chipped.
    const html = renderBare("#1 first, #2 next", "w-eng", [bareTask({ task_number: 1 })], () => {});
    expect(html).not.toContain('data-task-ref="1"');
    expect(html).not.toContain("silverhand-task-bare:1");
    expect(html).toContain("#1");
  });

  it("leaves bare `#10` as plain text (upper boundary of the excluded range)", () => {
    const html = renderBare(
      "See #10 in the list.",
      "w-eng",
      [bareTask({ task_number: 10 })],
      () => {},
    );
    expect(html).not.toContain('data-task-ref="10"');
    expect(html).not.toContain("silverhand-task-bare:10");
    expect(html).toContain("#10");
  });

  it("chips bare `#11` when resolved (lower boundary of the matched range)", () => {
    const html = renderBare("Refs #11 today.", "w-eng", [bareTask({ task_number: 11 })], () => {});
    expect(html).toContain('data-task-ref="11"');
    expect(html).toContain('data-task-ref-state="resolved"');
  });

  it("preserves explicit `#task:1`–`#task:10` (only the bare form is filtered)", () => {
    const html = renderBare("Working on #task:1 and #task:10 now.", "w-eng", [], () => {});
    expect(html).toContain("#task:1");
    expect(html).toContain("#task:1");
    expect(html).toContain("#task:10");
    expect(html).toContain("#task:10");
  });
});

describe("Markdown mermaid fences (#task:1322)", () => {
  const DIAGRAM = "```mermaid\ngraph TD; A-->B;\n```";

  function renderInteractive(content: string): string {
    return renderToStaticMarkup(
      React.createElement(
        QueryClientProvider,
        { client: new QueryClient({ defaultOptions: { queries: { retry: false } } }) },
        React.createElement(Markdown, { content }),
      ),
    );
  }

  function renderExport(content: string): string {
    return renderToStaticMarkup(
      React.createElement(
        QueryClientProvider,
        { client: new QueryClient({ defaultOptions: { queries: { retry: false } } }) },
        React.createElement(Markdown, { content, interactive: false }),
      ),
    );
  }

  function renderExportWithSvg(
    content: string,
    mermaidSvgByCode: ReadonlyMap<string, string>,
  ): string {
    return renderToStaticMarkup(
      React.createElement(
        QueryClientProvider,
        { client: new QueryClient({ defaultOptions: { queries: { retry: false } } }) },
        React.createElement(Markdown, {
          content,
          interactive: false,
          mermaidSvgByCode,
        }),
      ),
    );
  }

  it("routes a ```mermaid fence to the diagram container, not a code block", () => {
    // SSR can't run the async render effect, so we assert the wiring: the
    // fence mounts MermaidDiagram (pending state) rather than a <CodeBlock>.
    // The probe is the render STATE, not a class: the live diagram wears the
    // code block's own chrome now (#task:2114), so "which component mounted"
    // can only be read off the state attribute.
    const html = renderInteractive(DIAGRAM);
    expect(html).toContain('data-mermaid-state="pending"');
    // At the untouched 100% zoom, the Mermaid block must not take vertical
    // wheel ownership away from the message stream (#task:2129).
    expect(html).toContain('data-mermaid-scroll="document"');
    // Not the interactive code-block chrome (that path offers copy; the
    // diagram view offers zoom + the source toggle instead).
    expect(html).not.toContain("code-block__copy");
  });

  it("detects the language case-insensitively (```Mermaid with capital M)", () => {
    // `mermaid` is a proper noun; a stray capital must not demote the diagram
    // to a plain code block.
    const html = renderInteractive("```Mermaid\ngraph TD; A-->B;\n```");
    expect(html).toContain('data-mermaid-state="pending"');
    expect(html).not.toContain("code-block__copy");
  });

  it("a non-mermaid fence still renders the interactive code block", () => {
    const html = renderInteractive("```ts\nconst x = 1;\n```");
    expect(html).toContain("code-block__copy");
    expect(html).not.toContain("mermaid-diagram");
  });

  it("export path renders mermaid source as a plain code block (screenshot-safe)", () => {
    // The export tree is captured synchronously; async diagram render would be
    // blank, so the source is shown instead. No diagram container, no copy chrome.
    const html = renderExport(DIAGRAM);
    expect(html).toContain('class="code-block"');
    expect(html).not.toContain("mermaid-diagram");
    expect(html).not.toContain("code-block__copy");
    expect(html).toContain("graph TD");
  });

  it("export path injects a pre-rendered SVG when one is supplied (#task:1322)", () => {
    // ImagePreviewModal pre-renders diagrams before capture and passes the SVGs
    // in, keyed by trimmed source. That diagram then renders as an SVG instead
    // of falling back to source, so the shared image shows the real diagram.
    const svgByCode = new Map([["graph TD; A-->B;", '<svg data-testid="prerendered"></svg>']]);
    const html = renderExportWithSvg(DIAGRAM, svgByCode);
    expect(html).toContain('class="mermaid-diagram"');
    expect(html).toContain('data-mermaid-state="ok"');
    expect(html).toContain('data-testid="prerendered"');
    // No source fallback chrome this time.
    expect(html).not.toContain('class="code-block"');
  });

  it("export path falls back to source when the diagram is absent from the map", () => {
    // A diagram that failed or timed out pre-render is simply not in the map;
    // the export must still show its source, never a blank — "always produces
    // an image" is bounded, not a bet on every render succeeding.
    const html = renderExportWithSvg(DIAGRAM, new Map());
    expect(html).toContain('class="code-block"');
    expect(html).not.toContain("mermaid-diagram");
    expect(html).toContain("graph TD");
  });
});

describe("oversized-content guard (#task:1550)", () => {
  // The default budget is the message content cap (chat bodies were bounded
  // there on the way in); the workspace file preview opts out entirely
  // (shiki 2026-07-26). Import the bound rather than restating a number, so the
  // assertions stay about BEHAVIOUR — heavy pipeline bypassed, preview
  // hard-bounded — not about copy or a magic constant.
  const CAP = MESSAGE_RENDER_BUDGET_BYTES;

  it("renders over-budget content as a bounded plain preview, never through the markdown pipeline", () => {
    const content = "# heading\n```js\ncode\n```\n" + "x".repeat(CAP);
    const html = render(content);
    // Heavy pipeline untouched: no heading, no code-block chrome.
    expect(html).not.toContain("<h1");
    expect(html).not.toContain('class="code-block"');
    // Preview is hard-bounded (4000 chars + chrome), not the full body.
    expect(html.length).toBeLessThan(10_000);
  });

  it("renders exactly-at-budget content through the normal markdown pipeline", () => {
    const pad = "a".repeat(CAP - "# t\n\n".length);
    const content = "# t\n\n" + pad;
    const html = render(content);
    expect(html).toContain("<h1");
  });

  it("measures the budget in UTF-8 bytes, not UTF-16 code units", () => {
    // CJK: 3 bytes per char — under the budget in code units, over it in bytes.
    const html = render("# t\n\n" + "汉".repeat(CAP / 3));
    expect(html).not.toContain("<h1");
  });

  it("renders an unbounded surface (workspace file preview) past the message budget", () => {
    // The daemon reads up to its own cap and appends a truncation line, so a
    // large note arrives well over the message cap. Under the old shared guard
    // this exact shape degraded to plain text (a 97KB work-log.md showed 4000
    // chars); the file preview is bounded upstream, not here.
    const preview = "# work log\n\n" + "a".repeat(CAP * 2) + "\n\n… (truncated)";
    const html = renderToStaticMarkup(
      React.createElement(Markdown, {
        content: preview,
        renderBudget: "unbounded",
      }),
    );
    expect(html).toContain("<h1");
  });
});

/**
 * #task:2120 — the locator, end to end through the real pipeline.
 *
 * WHY THIS FILE AND NOT THE UNIT TEST. Between the remark pass and the `a` override
 * the destination is rewritten: `mdast-util-to-hast` runs `normalizeUri`, which
 * percent-encodes every byte outside its ASCII set — so for a locator naming a CJK
 * channel, or a name containing `^`, the href the component receives is a DIFFERENT
 * STRING from the one the author typed, and the canonical grammar correctly refuses
 * that second spelling. `reference-href.test.ts` asserts the decision reads the
 * authored bytes; only a real parse can assert that those bytes SURVIVE the pipeline
 * and that the surviving pair is the one the grammar sees (@Cindy, review point 3).
 *
 * Every chip here is INERT, and that is the environment rather than a ruling: SSR
 * mounts no query, so nothing resolves and `resolveLocatorTarget` returns `null`. What
 * these assertions own is therefore existence and words — whether a coordinate became
 * a chip at all, and which text it drew — which is exactly the half the byte question
 * decides. Navigability itself is asserted in `locator-target.test.ts`, where it is
 * decided.
 */
describe("Markdown locators (#task:2120)", () => {
  function renderLocator(content: string, interactive: boolean): string {
    const qc = new QueryClient({
      defaultOptions: { queries: { retry: false } },
    });
    return renderToStaticMarkup(
      React.createElement(
        QueryClientProvider,
        { client: qc },
        React.createElement(
          StaticRouter,
          { location: "/" },
          React.createElement(Markdown, { content, interactive }),
        ),
      ),
    );
  }

  for (const interactive of [true, false]) {
    const branch = interactive ? "live" : "export";

    it(`draws a channel coordinate whose name the href can no longer spell (${branch})`, () => {
      const html = renderLocator(
        "see <silverhand://workspace/silverhand/UI%2FUX%20优化>",
        interactive,
      );

      expect(html).not.toContain("data-locator-ref=");
      expect(html).toContain("UI%2FUX%20优化");
      expect(html).toContain("silverhand");
      expect(html).toContain("silverhand://");
      expect(html).not.toContain("<a ");
    });

    it(`draws a coordinate for a name normalizeUri would escape (${branch})`, () => {
      const html = renderLocator("see <silverhand://workspace/a^b>", interactive);

      expect(html).not.toContain("data-locator-ref=");
      expect(html).toContain("a^b");
      expect(html).not.toContain("<a ");
    });

    it(`refuses the over-escaped spelling of that same name (${branch})`, () => {
      const html = renderLocator("see <silverhand://workspace/a%5Eb>", interactive);

      expect(html).not.toContain("data-locator-ref=");
      expect(html).toContain("silverhand://workspace/a%5Eb");
      expect(html).not.toContain("<a ");
    });

    it(`draws every segment a thread coordinate spells, ending in the word (${branch})`, () => {
      const html = renderLocator(
        "see <silverhand://workspace/silverhand/design/BmgJYofni2Q9Glth>",
        interactive,
      );

      expect(html).not.toContain("data-locator-ref=");
      expect(html).toContain("design");
      expect(html).toContain("BmgJYofni2Q9Glth");
      expect(html).toContain("BmgJYofni2Q9Glth");
    });

    it(`draws a DM coordinate with both handles, joined (${branch})`, () => {
      const html = renderLocator("see <silverhand://dm/Cindy/shiki>", interactive);

      expect(html).not.toContain("data-locator-ref=");
      expect(html).toContain("silverhand://dm/Cindy/shiki");
      expect(html).not.toContain("<a ");
    });

    it(`ignores a label-carrying link to a locator (${branch})`, () => {
      const html = renderLocator("[here](silverhand://workspace/silverhand)", interactive);

      expect(html).toContain("here");
      expect(html).not.toContain("data-locator-ref=");
      expect(html).not.toContain("<a ");
    });

    it(`does not draw a coordinate written inside a code span (${branch})`, () => {
      const html = renderLocator("write `<silverhand://workspace/silverhand>`", interactive);

      expect(html).toContain("silverhand://workspace/silverhand");
      expect(html).not.toContain("data-locator-ref=");
    });

    it(`leaves a destination neither grammar accepts as its own text (${branch})`, () => {
      for (const href of [
        "silverhand://workspace",
        "silverhand://workspace/",
        "silverhand://workspace/a/b/c/d",
        "silverhand://dm/Cindy",
        "silverhand://dm/shiki/shiki",
        "silverhand://Workspace/x",
        "silverhand://workspace/silverhand/design/short",
      ]) {
        const html = renderLocator(`see <${href}> here`, interactive);
        expect(html, href).not.toContain("data-locator-ref=");
        expect(html, href).not.toContain("data-mention-kind=");
        expect(html, href).not.toContain("<a ");
      }
    });

    it(`preserves an unresolvable coordinate as raw text (${branch})`, () => {
      const html = renderLocator("see <silverhand://workspace/nobody>", interactive);

      expect(html).not.toContain("data-locator-ref=");
      expect(html).toContain("nobody");
      expect(html).not.toContain('role="button"');
    });
  }
});
