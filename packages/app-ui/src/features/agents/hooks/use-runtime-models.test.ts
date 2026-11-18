import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { QueryClient, QueryObserver } from "@tanstack/react-query";
import { api } from "@/lib/api";
import {
  modelReasoningOptions,
  runtimeModelsKey,
  runtimeModelsQueryOptions,
} from "./use-runtime-models";

vi.mock("@/lib/api", () => ({ api: vi.fn() }));
let client: QueryClient;
beforeEach(() => {
  vi.clearAllMocks();
  client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
});
afterEach(() => client.clear());
const response = (runtime: string, models: string[]) => ({
  runtimes: [],
  detected_at: 1,
  model_discovery: { runtime, status: "ok", models, detected_at: 1, cached: false },
});

it("scopes native suggestions to the selected host and runtime, separately from installation data", async () => {
  vi.mocked(api)
    .mockResolvedValueOnce(response("pi", ["p/a"]))
    .mockResolvedValueOnce(response("pi", ["p/b"]))
    .mockResolvedValueOnce(response("codex", ["c"]));
  await client.fetchQuery(runtimeModelsQueryOptions("host-a", "pi"));
  await client.fetchQuery(runtimeModelsQueryOptions("host-b", "pi"));
  await client.fetchQuery(runtimeModelsQueryOptions("host-a", "codex"));
  expect(client.getQueryData(runtimeModelsKey("host-a", "pi"))).toMatchObject({ models: ["p/a"] });
  expect(client.getQueryData(runtimeModelsKey("host-b", "pi"))).toMatchObject({ models: ["p/b"] });
  expect(client.getQueryData(runtimeModelsKey("host-a", "codex"))).toMatchObject({ models: ["c"] });
  expect(vi.mocked(api).mock.calls.map(([url]) => url)).toEqual([
    "/api/hosts/host-a/runtime-catalog?models_for=pi",
    "/api/hosts/host-b/runtime-catalog?models_for=pi",
    "/api/hosts/host-a/runtime-catalog?models_for=codex",
  ]);
});

it("old servers can return the ordinary catalog; failure and unsupported do not become an empty successful list", async () => {
  vi.mocked(api).mockResolvedValueOnce({ runtimes: [], detected_at: 1 });
  expect(await client.fetchQuery(runtimeModelsQueryOptions("host", "pi"))).toBeNull();
  vi.mocked(api).mockRejectedValueOnce(new Error("old daemon refuses new parameter"));
  await expect(
    client.fetchQuery(runtimeModelsQueryOptions("host", "pi", true, true)),
  ).rejects.toThrow();
  expect(client.getQueryState(runtimeModelsKey("host", "pi"))?.status).toBe("error");
});

it("rejects a response for another runtime instead of caching its models", async () => {
  vi.mocked(api).mockResolvedValue(response("grok", ["wrong"]));
  await expect(client.fetchQuery(runtimeModelsQueryOptions("host", "pi"))).rejects.toThrow(
    "runtime mismatch",
  );
  expect(client.getQueryData(runtimeModelsKey("host", "pi"))).toBeUndefined();
});

it("force refresh bypasses cached discovery while leaving other hosts untouched", async () => {
  vi.mocked(api)
    .mockResolvedValueOnce(response("pi", ["old"]))
    .mockResolvedValueOnce(response("pi", ["new"]));
  await client.fetchQuery(runtimeModelsQueryOptions("host", "pi"));
  await client.fetchQuery(runtimeModelsQueryOptions("host", "pi"));
  expect(api).toHaveBeenCalledTimes(1);
  await client.fetchQuery(runtimeModelsQueryOptions("host", "pi", true, true));
  expect(api).toHaveBeenLastCalledWith(
    "/api/hosts/host/runtime-catalog?models_for=pi&force=1",
    expect.anything(),
  );
  expect(client.getQueryData(runtimeModelsKey("host", "pi"))).toMatchObject({ models: ["new"] });
});

it("Qoder and unavailable hosts never start discovery", async () => {
  const observers = [
    runtimeModelsQueryOptions("host", "qoder"),
    runtimeModelsQueryOptions(null, "pi"),
    runtimeModelsQueryOptions("host", "pi", false),
  ].map((options) => new QueryObserver(client, options));
  const releases = observers.map((observer) => observer.subscribe(() => {}));
  await new Promise((resolve) => setTimeout(resolve, 10));
  expect(api).not.toHaveBeenCalled();
  releases.forEach((release) => release());
});

it("uses discovered per-model efforts, including an explicit empty set, else the runtime vocabulary", () => {
  const discovery = {
    runtime: "codex" as const,
    status: "ok" as const,
    models: ["custom", "plain"],
    detected_at: 1,
    cached: false,
    default_model: "custom",
    reasoning_by_model: {
      custom: { supported_efforts: ["high", "future-tier"], default_effort: "high" },
      plain: { supported_efforts: [] },
    },
  };
  const codex = ["low", "medium", "high", "xhigh", "max", "ultra"];
  expect(modelReasoningOptions("codex", "custom", discovery)).toEqual({
    options: ["high", "future-tier"],
    defaultEffort: "high",
  });
  expect(modelReasoningOptions("codex", null, discovery).options).toEqual(["high", "future-tier"]);
  expect(modelReasoningOptions("codex", "plain", discovery).options).toEqual([]);
  expect(modelReasoningOptions("codex", "unlisted", discovery).options).toEqual(codex);
  expect(modelReasoningOptions("codex", "custom", null).options).toEqual(codex);
  expect(
    modelReasoningOptions("claude", null, {
      ...discovery,
      runtime: "claude",
      default_model: "opus",
      reasoning_by_model: { opus: { supported_efforts: ["max"] } },
    }).options,
  ).toEqual(["max"]);
  expect(modelReasoningOptions("qoder", "custom", null).options).toEqual([
    "none",
    "low",
    "medium",
    "high",
    "xhigh",
    "max",
  ]);
});
