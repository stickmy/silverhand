import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { focusManager, QueryClient, QueryObserver } from "@tanstack/react-query";
import { api } from "@/lib/api";
import { agentAvailabilityQueryOptions, agentPresenceQueryOptions } from "./hooks";

vi.mock("@/lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@/lib/api")>()),
  api: vi.fn(),
}));

let client: QueryClient;

beforeEach(() => {
  vi.mocked(api).mockReset();
  client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  client.mount();
});

afterEach(() => {
  client.clear();
  client.unmount();
  focusManager.setFocused(undefined);
});

function seedAgentRead(view: "public" | "privileged"): void {
  client.setQueryData(["agents", "a1"], { view, agent: { id: "a1" } });
}

async function availabilityAfterFocus(): Promise<string | undefined> {
  vi.mocked(api).mockResolvedValue({ availability: "offline" });
  const observer = new QueryObserver(client, agentAvailabilityQueryOptions(client, "a1"));
  const unsubscribe = observer.subscribe(() => {});
  await vi.waitFor(() => expect(observer.getCurrentResult().data).toBe("offline"));

  vi.mocked(api).mockResolvedValue({ availability: "online" });
  focusManager.setFocused(false);
  focusManager.setFocused(true);
  await new Promise((resolve) => setTimeout(resolve, 20));
  const data = observer.getCurrentResult().data;
  unsubscribe();
  return data;
}

describe("event-driven agent reads on window focus", () => {
  it("re-read an agent the viewer sees at the public tier", async () => {
    seedAgentRead("public");

    expect(await availabilityAfterFocus()).toBe("online");
  });

  it("leave a privileged agent to its realtime frames", async () => {
    seedAgentRead("privileged");

    expect(await availabilityAfterFocus()).toBe("offline");
  });

  it("re-read presence for a public agent", async () => {
    seedAgentRead("public");
    vi.mocked(api).mockResolvedValue({ away_at: null, server_now: 1 });
    const observer = new QueryObserver(client, agentPresenceQueryOptions(client, "a1"));
    const unsubscribe = observer.subscribe(() => {});
    await vi.waitFor(() => expect(observer.getCurrentResult().isSuccess).toBe(true));

    focusManager.setFocused(false);
    focusManager.setFocused(true);
    await vi.waitFor(() => expect(api).toHaveBeenCalledTimes(2));
    unsubscribe();
  });
});
