import { describe, expect, it, vi } from "vitest";
import {
  executeMessageJump,
  type MessageJumpEnvironment,
} from "./features/conversation-shell/message-jump";
import type { DirectPairAuthority } from "./features/app-shell/chat-place-restore";

function environment(pairs: DirectPairAuthority, workspace: string | null = "w") {
  const calls: string[] = [];
  const env: MessageJumpEnvironment = {
    section: "chat",
    pairs,
    resolveWorkspace: vi.fn(() => {
      calls.push("resolve");
      return workspace;
    }),
    prime: vi.fn(() => {
      calls.push("prime");
    }),
    navigate: vi.fn(() => {
      calls.push("navigate");
    }),
  };
  return { env, calls };
}
const target = { channelId: "c", messageId: "m" };

describe("message jump side-effect boundary", () => {
  it.each<DirectPairAuthority>([
    { status: "pending" },
    { status: "failed" },
    { status: "refetching", conversationIds: new Set() },
  ])("refuses unknown pair authority without prime or navigation: $status", (pairs) => {
    const { env, calls } = environment(pairs);
    expect(executeMessageJump(target, env)).toBe(false);
    expect(calls).toEqual([]);
  });

  it("does not prime when the final workspace is unresolved", () => {
    const { env, calls } = environment({ status: "ready", conversationIds: new Set() }, null);
    expect(executeMessageJump(target, env)).toBe(false);
    expect(calls).toEqual(["resolve"]);
  });

  it("resolves a channel before priming and navigating exactly once", () => {
    const { env, calls } = environment({ status: "pending" });
    expect(executeMessageJump({ ...target, workspaceId: "w" }, env)).toBe(true);
    expect(calls).toEqual(["resolve", "prime", "navigate"]);
    expect(env.navigate).toHaveBeenCalledWith("/chat/w/w/c/c");
  });

  it("keeps a legal DM thread inside Activity", () => {
    const { env, calls } = environment({ status: "ready", conversationIds: new Set(["c"]) }, null);
    env.section = "activity";
    expect(executeMessageJump({ ...target, threadRootId: "root" }, env)).toBe(true);
    expect(calls).toEqual(["resolve", "prime", "navigate"]);
    expect(env.navigate).toHaveBeenCalledWith("/activity/d/c?pane=threads.root");
  });

  it("inline find only primes, even when authority and workspace are unavailable", () => {
    const { env, calls } = environment({ status: "pending" }, null);
    expect(executeMessageJump({ ...target, preserveSidePanel: true }, env)).toBe(false);
    expect(calls).toEqual(["prime"]);
  });
});
