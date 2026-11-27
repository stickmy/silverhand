import { describe, expect, it } from "vitest";
import { runtimeCatalogEntry, type AgentDto } from "@silverhand/shared";
import {
  buildSwitchPayload,
  buildTuningPatch,
  canSaveRuntimeConfig,
  describeSwitchToast,
  draftAfterRuntimeChange,
  draftFromAgent,
  draftIsDirty,
  modelDraftAfterCommit,
  routeRuntimeConfigSave,
  type RuntimeConfigDraft,
} from "./runtime-config-editor";
import { t } from "@/lib/i18n";

const baseAgent: AgentDto = {
  id: "a-1",
  host_id: "h-1",
  host_label: "MacBook",
  name: "alice",
  display_name: "Alice",
  runtime: "codex",
  session_scope: "conversation",
  model: "gpt-5-codex",
  reasoning_effort: "high",
  service_tier: "priority",
  avatar_url: null,
  description: null,
  created_by_user_id: "u-1",
  materialization_status: "active",
  materialization_failed_code: null,
  materialization_failed_at: null,
  deletion_started_at: null,
  created_at: 0,
  updated_at: 0,
};

describe("draftFromAgent", () => {
  it("hydrates every draft position from the persisted agent", () => {
    expect(draftFromAgent(baseAgent)).toEqual({
      runtime: "codex",
      modelChoice: "specify",
      modelValue: "gpt-5-codex",
      reasoning: "high",
      serviceTier: "priority",
    });
  });

  it("maps null model to `default`, null reasoning/service_tier to `__default__` (when supported)", () => {
    expect(
      draftFromAgent({
        ...baseAgent,
        model: null,
        reasoning_effort: null,
        service_tier: null,
      }),
    ).toEqual({
      runtime: "codex",
      modelChoice: "default",
      modelValue: "",
      reasoning: "__default__",
      serviceTier: "__default__",
    });
  });

  it("keeps Claude effort at the driver default and hides unsupported service tier", () => {
    const d = draftFromAgent({
      ...baseAgent,
      runtime: "claude",
      reasoning_effort: null,
      service_tier: null,
    });
    expect(d.reasoning).toBe("__default__");
    expect(d.serviceTier).toBe("");
  });
});

describe("draftAfterRuntimeChange", () => {
  it("resets tuning to the unset state when the target differs from the agent runtime", () => {
    const next = draftAfterRuntimeChange(baseAgent, "claude");
    expect(next).toEqual({
      runtime: "claude",
      modelChoice: "unset",
      modelValue: "",
      reasoning: "",
      serviceTier: "",
    });
  });

  it("re-hydrates from the agent when the target equals the current runtime (undo)", () => {
    // User picked a different runtime (draft is now blanked), then picked
    // the ORIGINAL back — should restore the persisted tuning, not stay
    // blank.
    const initial = draftFromAgent(baseAgent);
    const restored = draftAfterRuntimeChange(baseAgent, "codex");
    expect(restored).toEqual(initial);
  });
});

describe("canSaveRuntimeConfig", () => {
  it("rejects an unchanged draft (nothing to save)", () => {
    expect(canSaveRuntimeConfig(baseAgent, draftFromAgent(baseAgent))).toBe(false);
  });

  it("rejects a draft with unset modelChoice after a runtime switch", () => {
    const d = draftAfterRuntimeChange(baseAgent, "claude");
    expect(canSaveRuntimeConfig(baseAgent, d)).toBe(false);
  });

  it("rejects a `specify` model with blank value", () => {
    const d: RuntimeConfigDraft = {
      runtime: "claude",
      modelChoice: "specify",
      modelValue: "   ",
      reasoning: "",
      serviceTier: "",
    };
    expect(canSaveRuntimeConfig(baseAgent, d)).toBe(false);
  });

  it("rejects when target supports reasoning but reasoning is `''`", () => {
    const d: RuntimeConfigDraft = {
      runtime: "codex",
      modelChoice: "default",
      modelValue: "",
      reasoning: "",
      serviceTier: "__default__",
    };
    // Also has to be dirty; force by tweaking model relative to base.
    expect(canSaveRuntimeConfig(baseAgent, d)).toBe(false);
  });

  it("accepts an all-defaults runtime switch (every field explicitly chosen)", () => {
    const d: RuntimeConfigDraft = {
      runtime: "claude",
      modelChoice: "default",
      modelValue: "",
      reasoning: "__default__",
      serviceTier: "",
    };
    expect(canSaveRuntimeConfig(baseAgent, d)).toBe(true);
  });

  it("accepts a tuning-only diff (model changes, runtime unchanged)", () => {
    const d: RuntimeConfigDraft = {
      ...draftFromAgent(baseAgent),
      modelValue: "gpt-6-codex",
    };
    expect(canSaveRuntimeConfig(baseAgent, d)).toBe(true);
  });

  it("accepts a saved Codex effort independently of the static model catalog", () => {
    const compatible: RuntimeConfigDraft = {
      ...draftFromAgent(baseAgent),
      modelValue: "gpt-5.6-sol",
      reasoning: "ultra",
    };
    expect(canSaveRuntimeConfig(baseAgent, compatible)).toBe(true);
    expect(
      canSaveRuntimeConfig(baseAgent, {
        ...compatible,
        modelValue: "gpt-5.6-luna",
      }),
    ).toBe(true);
  });
});

describe("draftIsDirty", () => {
  it("false when draft matches agent", () => {
    expect(draftIsDirty(baseAgent, draftFromAgent(baseAgent))).toBe(false);
  });
  it("true when runtime changes", () => {
    expect(draftIsDirty(baseAgent, { ...draftFromAgent(baseAgent), runtime: "claude" })).toBe(true);
  });
  it("true when only tuning drifts", () => {
    expect(
      draftIsDirty(baseAgent, {
        ...draftFromAgent(baseAgent),
        serviceTier: "default",
      }),
    ).toBe(true);
  });
});

describe("routeRuntimeConfigSave", () => {
  it("routes a runtime change to the transition endpoint (switch)", () => {
    const d: RuntimeConfigDraft = {
      runtime: "claude",
      modelChoice: "default",
      modelValue: "",
      reasoning: "",
      serviceTier: "",
    };
    const r = routeRuntimeConfigSave(baseAgent, d);
    expect(r.kind).toBe("switch");
    if (r.kind !== "switch") return;
    expect(r.payload).toEqual({
      runtime: "claude",
      model: null,
      reasoning_effort: null,
      service_tier: null,
    });
  });

  it("routes a tuning-only diff to PATCH (only changed fields)", () => {
    const d: RuntimeConfigDraft = {
      ...draftFromAgent(baseAgent),
      serviceTier: "default",
    };
    const r = routeRuntimeConfigSave(baseAgent, d);
    expect(r.kind).toBe("patch");
    if (r.kind !== "patch") return;
    expect(r.payload).toEqual({ service_tier: "default" });
    // Runtime is `z.never()` in PATCH schema — must not be included.
    expect(r.payload).not.toHaveProperty("runtime");
  });

  it("returns `nothing` for an unchanged draft", () => {
    const r = routeRuntimeConfigSave(baseAgent, draftFromAgent(baseAgent));
    expect(r.kind).toBe("nothing");
  });
});

describe("buildSwitchPayload — supported/unsupported gating", () => {
  it("forces service_tier to null when target has no service tier", () => {
    const d: RuntimeConfigDraft = {
      runtime: "claude",
      modelChoice: "default",
      modelValue: "",
      // stale from a previous codex pick — must be forced null.
      reasoning: "",
      serviceTier: "priority",
    };
    expect(buildSwitchPayload(d).service_tier).toBeNull();
  });

  it("forces reasoning_effort to null when target has no reasoning support", () => {
    const d: RuntimeConfigDraft = {
      runtime: "opencode",
      modelChoice: "default",
      modelValue: "",
      reasoning: "high",
      serviceTier: "",
    };
    expect(buildSwitchPayload(d).reasoning_effort).toBeNull();
  });

  it("trims a specify model value", () => {
    const d: RuntimeConfigDraft = {
      runtime: "codex",
      modelChoice: "specify",
      modelValue: "  gpt-5-codex  ",
      reasoning: "high",
      serviceTier: "priority",
    };
    expect(buildSwitchPayload(d).model).toBe("gpt-5-codex");
  });
});

describe("buildTuningPatch — only diffed fields", () => {
  it("includes model=null when switching to driver default", () => {
    const d: RuntimeConfigDraft = {
      ...draftFromAgent(baseAgent),
      modelChoice: "default",
      modelValue: "",
    };
    expect(buildTuningPatch(baseAgent, d)).toEqual({ model: null });
  });

  it("omits fields that did not change", () => {
    // Only reasoning changes.
    const d: RuntimeConfigDraft = {
      ...draftFromAgent(baseAgent),
      reasoning: "low",
    };
    expect(buildTuningPatch(baseAgent, d)).toEqual({ reasoning_effort: "low" });
  });

  it("respects target capabilities — never patches unsupported fields", () => {
    const claudeAgent: AgentDto = {
      ...baseAgent,
      runtime: "claude",
      reasoning_effort: null,
      service_tier: null,
    };
    // A stale draft carries `serviceTier="priority"` from a previous UI state —
    // but claude doesn't support it, so it must never reach the patch.
    const d: RuntimeConfigDraft = {
      runtime: "claude",
      modelChoice: "specify",
      modelValue: "sonnet",
      reasoning: "",
      serviceTier: "priority",
    };
    const patch = buildTuningPatch(claudeAgent, d);
    expect(patch).toEqual({ model: "sonnet" });
    expect(patch).not.toHaveProperty("service_tier");
    expect(patch).not.toHaveProperty("reasoning_effort");
  });
});

describe("describeSwitchToast", () => {
  it("returns success tone on zero failures", () => {
    expect(describeSwitchToast("codex", 0).tone).toBe("success");
  });
  it("returns warning tone with singular for one failure", () => {
    const r = describeSwitchToast("claude", 1);
    expect(r.tone).toBe("warning");
    expect(r.message).toMatch(/1 conversation\b/);
  });
  it("returns warning tone with plural for many failures", () => {
    const r = describeSwitchToast("claude", 3);
    expect(r.tone).toBe("warning");
    expect(r.message).toMatch(/3 conversations/);
  });
});

describe("modelDraftAfterCommit — unified Combobox commit mapping", () => {
  const seed: RuntimeConfigDraft = {
    runtime: "codex",
    modelChoice: "unset",
    modelValue: "",
    reasoning: "__default__",
    serviceTier: "__default__",
  };

  it("picks driver default when the dropdown sentinel comes through", () => {
    expect(modelDraftAfterCommit(seed, t("agents.useDriverDefault"))).toEqual({
      ...seed,
      modelChoice: "default",
      modelValue: "",
    });
  });
  it("specify + empty commit → unset (user cleared what they typed)", () => {
    const specifying: RuntimeConfigDraft = {
      ...seed,
      modelChoice: "specify",
      modelValue: "gpt-5-codex",
    };
    expect(modelDraftAfterCommit(specifying, "")).toEqual({
      ...specifying,
      modelChoice: "unset",
      modelValue: "",
    });
  });
  it("default + empty commit → stays default (bare focus→blur must not demote)", () => {
    // Regression lock for #1472 round-3 Cindy HIGH: Combobox onBlur fires
    // onCommit(''), and a default draft's input value is empty by design
    // (placeholder = "Using driver default"). Without the guard, a user
    // who just clicks into the Model input and clicks out would silently
    // lose their driver-default pick.
    const defaulting: RuntimeConfigDraft = {
      ...seed,
      modelChoice: "default",
      modelValue: "",
    };
    expect(modelDraftAfterCommit(defaulting, "")).toEqual(defaulting);
  });
  it("unset + empty commit → stays unset (no-op)", () => {
    expect(modelDraftAfterCommit(seed, "")).toEqual(seed);
  });
  it("whitespace-only commit follows the same rule (trimmed empty)", () => {
    const defaulting: RuntimeConfigDraft = {
      ...seed,
      modelChoice: "default",
    };
    // Whitespace-only from `default` still preserves default — trimmed
    // empty is empty for state-transition purposes too.
    expect(modelDraftAfterCommit(defaulting, "   ")).toEqual(defaulting);
    const specifying: RuntimeConfigDraft = {
      ...seed,
      modelChoice: "specify",
      modelValue: "x",
    };
    expect(modelDraftAfterCommit(specifying, "   ")).toEqual({
      ...specifying,
      modelChoice: "unset",
      modelValue: "",
    });
  });
  it("any other value goes to specify with the trimmed model id", () => {
    expect(modelDraftAfterCommit(seed, "  gpt-4o  ")).toEqual({
      ...seed,
      modelChoice: "specify",
      modelValue: "gpt-4o",
    });
  });
  it("preserves unrelated draft fields (runtime, reasoning, serviceTier) verbatim", () => {
    const before: RuntimeConfigDraft = {
      runtime: "claude",
      modelChoice: "default",
      modelValue: "",
      reasoning: "high",
      serviceTier: "",
    };
    expect(modelDraftAfterCommit(before, "sonnet")).toEqual({
      runtime: "claude",
      modelChoice: "specify",
      modelValue: "sonnet",
      reasoning: "high",
      serviceTier: "",
    });
  });

  it("preserves explicit effort when changing a native-runtime model", () => {
    const ultra: RuntimeConfigDraft = {
      runtime: "codex",
      modelChoice: "specify",
      modelValue: "gpt-5.6-sol",
      reasoning: "ultra",
      serviceTier: "__default__",
    };
    expect(modelDraftAfterCommit(ultra, "gpt-5.6-luna")).toEqual({
      ...ultra,
      modelValue: "gpt-5.6-luna",
      reasoning: "ultra",
    });
    expect(modelDraftAfterCommit(ultra, "gpt-5.6-terra")).toEqual({
      ...ultra,
      modelValue: "gpt-5.6-terra",
      reasoning: "ultra",
    });
  });
});

describe("canSaveRuntimeConfig / modelDraftAfterCommit — native effort ownership", () => {
  it("an explicit effort can be saved for any Codex model", () => {
    const solUltra: RuntimeConfigDraft = {
      runtime: "codex",
      modelChoice: "specify",
      modelValue: "gpt-5.6-sol",
      reasoning: "ultra",
      serviceTier: "priority",
    };
    expect(canSaveRuntimeConfig(baseAgent, solUltra)).toBe(true);
    expect(
      canSaveRuntimeConfig(baseAgent, {
        ...solUltra,
        modelValue: "gpt-5.6-terra",
      }),
    ).toBe(true);
  });

  it("model changes do not silently clear a saved effort", () => {
    const ultra: RuntimeConfigDraft = {
      runtime: "codex",
      modelChoice: "specify",
      modelValue: "gpt-5.6-sol",
      reasoning: "ultra",
      serviceTier: "priority",
    };
    const committed = modelDraftAfterCommit(ultra, "gpt-5.6-terra");
    expect(committed.modelValue).toBe("gpt-5.6-terra");
    expect(committed.reasoning).toBe("ultra");
  });

  it("baseline High stays valid across a model commit", () => {
    const committed = modelDraftAfterCommit(draftFromAgent(baseAgent), "gpt-5.6-terra");
    expect(committed.reasoning).toBe("high");
    expect(
      canSaveRuntimeConfig(baseAgent, {
        runtime: "codex",
        modelChoice: "specify",
        modelValue: "gpt-5.6-terra",
        reasoning: "high",
        serviceTier: "priority",
      }),
    ).toBe(true);
  });
});

// Source fence — the root editor <div> must own Escape at capture phase.
// A bubble-phase handler is swallowed by Radix Select.Trigger before it
// reaches the root, trapping Esc when focus sits on any Select.
describe("RuntimeConfigEditor source invariants", () => {
  it("owns Escape at capture phase so Radix Select cannot swallow it", async () => {
    const { readFileSync } = await import("node:fs");
    const { fileURLToPath } = await import("node:url");
    const path = await import("node:path");
    const src = readFileSync(
      path.join(fileURLToPath(new URL("./runtime-config-editor.tsx", import.meta.url))),
      "utf8",
    );
    // Same capture handler must branch on `Escape` and call `cancel()` —
    // a stray capture handler that doesn't touch Esc would otherwise let
    // the assertion pass.
    expect(src).toMatch(
      /onKeyDownCapture=\{[\s\S]*?e\.key\s*===\s*["']Escape["'][\s\S]*?cancel\(\)/,
    );
    expect(src).not.toMatch(/onKeyDown=\{[^}]*Escape/);
    // Root capture must skip while the confirm dialog is open, otherwise
    // Esc inside the dialog would fall through to `cancel()` and drop the
    // editor draft. Radix Dialog owns Escape when `pendingSwitch !== null`.
    expect(src).toMatch(/onKeyDownCapture=\{[\s\S]*?pendingSwitch\s*===\s*null[\s\S]*?Escape/);
  });

  it("routes `switch` through `pendingSwitch` state (does not mutate inline)", async () => {
    const { readFileSync } = await import("node:fs");
    const { fileURLToPath } = await import("node:url");
    const path = await import("node:path");
    const src = readFileSync(
      path.join(fileURLToPath(new URL("./runtime-config-editor.tsx", import.meta.url))),
      "utf8",
    );
    // onSave's switch branch stashes the payload rather than firing the
    // mutation directly.
    expect(src).toMatch(
      /route\.kind\s*===\s*["']switch["'][\s\S]*?setPendingSwitch\(route\.payload\)/,
    );
    // The mutation is only invoked from the confirm path (runSwitch).
    expect(src).toMatch(/async function runSwitch[\s\S]*?switchMut\.mutateAsync\(payload\)/);
    // Dialog gate visibility ties to `pendingSwitch !== null`.
    expect(src).toMatch(/SwitchRuntimeConfirmDialog[\s\S]*?open=\{pendingSwitch\s*!==\s*null\}/);
    // Switch errors flow into the dialog so they render above the modal
    // instead of behind it.
    expect(src).toMatch(/SwitchRuntimeConfirmDialog[\s\S]*?errorMessage=\{switchErrorMessage\}/);
    // Focus must return to the Runtime Select trigger when the dialog
    // closes, otherwise focus drops to `<body>` and the parent-capture
    // Escape path stops receiving keydown events.
    expect(src).toMatch(/SwitchRuntimeConfirmDialog[\s\S]*?returnFocusRef=\{runtimeTriggerRef\}/);
  });

  it("P1-2: catalog / availability probe LIFECYCLE is edit-only", async () => {
    const { readFileSync } = await import("node:fs");
    const { fileURLToPath } = await import("node:url");
    const path = await import("node:path");
    const src = readFileSync(
      path.join(fileURLToPath(new URL("./runtime-config-editor.tsx", import.meta.url))),
      "utf8",
    );
    // Save gates on the static product table via the pure helper (2-arg, no
    // host catalog threaded in).
    expect(src).toMatch(/canSaveRuntimeConfig\(agent,\s*draft\)/);
    // A readonly profile view must never fetch availability / the runtime
    // catalog — both are gated on `editing` at the query `enabled` and the
    // availability host list.
    expect(src).toMatch(/enabled:\s*editing\s*&&\s*allSettled\s*&&\s*hostAvailable/);
    expect(src).toMatch(/editing\s*\?\s*\[agent\.host_id\]\s*:\s*\[\]/);
  });
});

describe("Pi runtime configuration", () => {
  const draft: RuntimeConfigDraft = {
    runtime: "pi",
    modelChoice: "default",
    modelValue: "",
    reasoning: "__default__",
    serviceTier: "",
  };

  it("serializes CLI defaults and discards unsupported service tier", () => {
    expect(canSaveRuntimeConfig(baseAgent, draft)).toBe(true);
    expect(buildSwitchPayload({ ...draft, serviceTier: "priority" })).toEqual({
      runtime: "pi",
      model: null,
      reasoning_effort: null,
      service_tier: null,
    });
    expect(runtimeCatalogEntry("pi").supportsProfileMcp).toBe(false);
    expect(runtimeCatalogEntry("pi").serviceTierOptions).toEqual([]);
  });

  it.each(["off", "minimal", "low", "medium", "high"])(
    "serializes %s reasoning with a free-text model",
    (reasoning) => {
      const configured = {
        ...draft,
        modelChoice: "specify" as const,
        modelValue: " provider/model-id ",
        reasoning,
      };
      expect(canSaveRuntimeConfig(baseAgent, configured)).toBe(true);
      expect(buildSwitchPayload(configured)).toEqual({
        runtime: "pi",
        model: "provider/model-id",
        reasoning_effort: reasoning,
        service_tier: null,
      });
    },
  );

  it("leaves effort support to the CLI and resets tuning across runtime changes", () => {
    expect(canSaveRuntimeConfig(baseAgent, { ...draft, reasoning: "ultra" })).toBe(true);
    expect(draftAfterRuntimeChange(baseAgent, "pi")).toEqual({
      runtime: "pi",
      modelChoice: "unset",
      modelValue: "",
      reasoning: "",
      serviceTier: "",
    });
    expect(draftAfterRuntimeChange(baseAgent, "codex")).toEqual(draftFromAgent(baseAgent));
  });

  it("patches an existing Pi agent without carrying unsupported tuning", () => {
    const agent = {
      ...baseAgent,
      runtime: "pi" as const,
      model: null,
      reasoning_effort: null,
      service_tier: null,
    };
    const next = { ...draftFromAgent(agent), reasoning: "minimal" };
    expect(canSaveRuntimeConfig(agent, next)).toBe(true);
    expect(buildTuningPatch(agent, next)).toEqual({ reasoning_effort: "minimal" });
  });
});

it("preserves Claude effort in saved drafts and PATCH payloads", () => {
  const agent = {
    ...baseAgent,
    runtime: "claude" as const,
    model: "custom",
    reasoning_effort: "future-tier",
    service_tier: null,
  };
  const draft = draftFromAgent(agent);
  expect(draft.reasoning).toBe("future-tier");
  const changed = modelDraftAfterCommit(draft, "another-custom");
  expect(changed.reasoning).toBe("future-tier");
  expect(canSaveRuntimeConfig(agent, changed)).toBe(true);
  expect(buildTuningPatch(agent, { ...changed, reasoning: "max" })).toMatchObject({
    model: "another-custom",
    reasoning_effort: "max",
  });
});
