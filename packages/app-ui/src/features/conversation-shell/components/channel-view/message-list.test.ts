import { beforeEach, describe, expect, it, vi } from "vitest";

// A parent-render probe: effects intentionally do NOT run before child props
// are inspected. Window bookkeeping in a layout effect would fail this test.
// Actual scrolling is verified separately in the browser.
const h = vi.hoisted(() => ({ slots: [] as unknown[], cursor: 0, retry: false, compact: true }));
vi.mock("react", async (load) => {
  const react = await load<typeof import("react")>();
  const useState = (initial: unknown) => {
    const slot = h.cursor++;
    if (!(slot in h.slots)) h.slots[slot] = typeof initial === "function" ? initial() : initial;
    return [
      h.slots[slot],
      (action: unknown) => {
        const next = typeof action === "function" ? action(h.slots[slot]) : action;
        if (!Object.is(next, h.slots[slot])) {
          h.slots[slot] = next;
          h.retry = true;
        }
      },
    ];
  };
  const useMemo = (fn: () => unknown, deps: unknown[]) => {
    const slot = h.cursor++;
    const previous = h.slots[slot] as { deps: unknown[]; value: unknown } | undefined;
    if (!previous || deps.some((dep, i) => !Object.is(dep, previous.deps[i]))) {
      h.slots[slot] = { deps, value: fn() };
    }
    return (h.slots[slot] as { value: unknown }).value;
  };
  return {
    ...react,
    useState,
    useReducer: (reducer: (s: unknown, e: unknown) => unknown, initial: unknown) => {
      const [state, set] = useState(initial);
      return [state, (event: unknown) => (set as Function)((s: unknown) => reducer(s, event))];
    },
    useRef: (initial: unknown) => useState(() => ({ current: initial }))[0],
    useMemo,
    useCallback: (fn: unknown, deps: unknown[]) => useMemo(() => fn, deps),
    useEffect: () => {},
    useLayoutEffect: () => {},
  };
});
vi.mock("@/navigation/layout", () => ({ useCompactLayout: () => h.compact }));
vi.mock("@/navigation/view-memory", () => ({ useViewMemory: () => null }));
vi.mock("@/features/messages/hooks/use-message-render-context", () => ({
  useMessageRenderContext: () => ({ liveChannels: [], archivedChannels: [] }),
}));
vi.mock("@/features/messages/hooks/author-avatar-hooks", () => ({
  useAuthorAvatarObjectUrls: () => ({}),
}));
vi.mock("@/features/messages/hooks/use-message-action-surface", () => ({
  useMessageActionSurface: () => ({}),
}));
vi.mock("@/features/messages/components/message-action-chrome", () => ({
  MessageActionChrome: () => null,
}));
vi.mock("@/features/messages/components/channel-message-item", () => ({
  ChannelMessageItem: () => null,
}));
vi.mock("@/features/messages/components/system-message", () => ({ SystemMessage: () => null }));
vi.mock("@/features/messages/components/system-activity-cluster", () => ({
  SystemActivityCluster: () => null,
}));
vi.mock("@/features/conversation-shell/components/conversation-view-skeleton", () => ({
  MessageRowsSkeleton: () => null,
}));
vi.mock("./message-scroller", () => ({ MessageScroller: () => null }));
import { MessageScroller } from "./message-scroller";
import { MessageList } from "./message-list";
import type { CachedMessage } from "@/features/channels/pending-message";
import { decodeMessageSeq } from "@silverhand/shared";

const rows = (start: number, count: number): CachedMessage[] =>
  Array.from({ length: count }, (_, i) => ({
    id: `m${start + i}`,
    from_kind: "user",
    author: null,
    created_at: start + i,
    seq: decodeMessageSeq(start + i),
    conversation_id: "a",
    from_agent_id: null,
    from_user_id: "u",
    content: "message",
    metadata: null,
    attachments: null,
    thread_root_id: null,
    linked_task: null,
    reactions: null,
  }));
const base = {
  channelId: "a",
  workspaceId: null,
  isLoading: false,
  hasMoreBefore: true,
  isFetchingEarlier: false,
  onLoadEarlier: vi.fn(),
  agentAvatarUrls: new Map(),
  jumpToMessageId: null,
  canReact: false,
};
function find(node: any): any {
  if (!node || typeof node !== "object") return;
  if (node.type === MessageScroller) return node;
  for (const child of [node.props?.children].flat(Infinity)) {
    const found = find(child);
    if (found) return found;
  }
}
function render(messages: CachedMessage[], overrides = {}) {
  let node;
  let attempts = 0;
  do {
    if (++attempts > 10) throw new Error("render loop");
    h.cursor = 0;
    h.retry = false;
    node = MessageList({ ...base, messages, ...overrides });
  } while (h.retry);
  return find(node);
}
beforeEach(() => {
  h.slots = [];
  h.cursor = 0;
  h.retry = false;
  h.compact = true;
});
describe("message window publication", () => {
  it("keeps the scroller mounted across prepends and head trims", () => {
    const before = render(rows(100, 60));
    const prepended = render(rows(70, 90));
    expect(prepended.props.items[0].message.id).toBe("m70");
    expect(prepended.key).toBe(before.key);
    const trimmed = render(rows(80, 80));
    expect(trimmed.key).toBe(before.key);
  });
  it("remounts a disjoint window at its own anchor", () => {
    const before = render(rows(100, 60));
    const replaced = render(rows(300, 60));
    expect(replaced.key).not.toBe(before.key);
    expect(replaced.props.initialAnchor).toEqual({ kind: "bottom" });
  });
  it("waits for a jump target before remounting, then centers on it", () => {
    const before = render(rows(100, 60));
    const pending = render(rows(300, 60), { jumpToMessageId: "m500" });
    expect(pending.key).toBe(before.key);
    const landed = render(rows(480, 60), { jumpToMessageId: "m500" });
    expect(landed.key).not.toBe(before.key);
    expect(landed.props.initialAnchor).toEqual({ kind: "key", key: "m500", align: "center" });
  });
  it("starts another conversation at a fresh mount", () => {
    render(rows(100, 60));
    render(rows(300, 60));
    const next = render(rows(200, 30), { channelId: "b" });
    expect(next.key).toBe("b:0");
  });
  it("does not remount when older system events merge into the first activity cluster", () => {
    const event = (id: number): CachedMessage => ({
      ...rows(id, 1)[0]!,
      from_kind: "system",
      metadata: { subtype: "task_edit" },
    });
    const messages = [event(100), ...rows(105, 30)];
    const before = render(messages);
    const after = render([event(99), ...messages]);
    expect(after.key).toBe(before.key);
    expect(after.props.items[0].messages).toHaveLength(2);
  });
  it("follows the bottom only at the tail with no jump in flight", () => {
    const messages = rows(100, 60);
    expect(render(messages).props.follow).toBe(true);
    expect(render(messages, { isAtTail: false }).props.follow).toBe(false);
    expect(render(messages, { jumpToMessageId: "m120" }).props.follow).toBe(false);
  });
  it("loads earlier history, but not when hidden, jumping, exhausted or fetching", () => {
    const onLoadEarlier = vi.fn();
    const messages = rows(100, 60);
    render(messages, { onLoadEarlier }).props.onReachTop();
    expect(onLoadEarlier).toHaveBeenCalledOnce();
    onLoadEarlier.mockClear();
    for (const blocked of [
      { surfaceActive: false },
      { jumpToMessageId: "m120" },
      { hasMoreBefore: false },
      { isFetchingEarlier: true },
    ]) {
      render(messages, { onLoadEarlier, ...blocked }).props.onReachTop();
    }
    expect(onLoadEarlier).not.toHaveBeenCalled();
  });
});
