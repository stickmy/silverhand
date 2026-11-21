import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { createElement, type ReactNode } from "react";
import { clusterMessagesKey, threadRootKey } from "@/features/channels/message-cache-core";
import { channelGateFor } from "@/features/channels/membership";
import { t } from "@/lib/i18n";
import { ThreadPanel } from "./thread-panel";

vi.mock("../use-thread-mark-read", () => ({ useThreadMarkRead: () => {} }));

// Expose the list's placeholder slot without mounting its virtual scroller.
vi.mock("./channel-view/message-list", () => ({
  MessageList: ({ emptyState }: { emptyState: ReactNode }) => emptyState,
}));

describe("thread pane with a loaded root and no reply rows", () => {
  async function renderPane(replyCount: number, failure: boolean) {
    const client = new QueryClient({
      defaultOptions: { queries: { retry: false, retryOnMount: false } },
    });
    client.setQueryData(threadRootKey("channel", "root"), {
      messages: [{ id: "root" }],
      reply_counts_by_root: { root: replyCount },
    });
    const key = clusterMessagesKey("channel", "root");
    if (failure) {
      await expect(
        client.fetchQuery({
          queryKey: key,
          queryFn: async () => {
            throw new Error("offline");
          },
        }),
      ).rejects.toThrow("offline");
    } else {
      client.setQueryData(key, {
        pages: [{ messages: [], has_more_after: false }],
        pageParams: [undefined],
      });
    }
    try {
      return renderToStaticMarkup(
        createElement(
          QueryClientProvider,
          { client },
          createElement(ThreadPanel, {
            channelId: "channel",
            rootMessageId: "root",
            workspaceId: null,
            directory: { kind: "withheld" },
            gate: channelGateFor("unknown"),
            onClose: () => {},
            onOpenAttachment: () => {},
          }),
        ),
      );
    } finally {
      client.clear();
    }
  }

  it.each([
    [116, true],
    [0, true],
    [116, false],
  ] as const)("offers retry for count %i and failed read %s", async (count, failure) => {
    const html = await renderPane(count, failure);
    expect(html).toContain(t("common.retry"));
    expect(html).not.toContain(t("conversationShell.noRepliesYet"));
  });
  it("shows an empty thread after a successful empty read", async () => {
    const html = await renderPane(0, false);
    expect(html).toContain(t("conversationShell.noRepliesYet"));
    expect(html).not.toContain(t("common.retry"));
  });
});
