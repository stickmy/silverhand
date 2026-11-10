import { type ReasoningEffort, type RuntimeKind } from './db/schema.js';

/**
 * Single source of truth for per-runtime UI facts.
 *
 * Before this existed, runtime `label` / selectability / proxy ordering
 * were hand-copied into four+ renderer sites (`CreateAgentDialog`,
 * `AgentProfileCard`, `HostDetailView` proxy form, `AgentMcpPane`), so
 * adding a runtime meant manually scanning for every copy — and qoder
 * was silently missed in create + profile the first time. This catalog
 * is the ubiquitous-language home for those facts; every consumer reads
 * from here. Renderer-only concerns (brand icon / tint colors) stay in
 * the renderer — this owns label / availability / capability, not pixels.
 */
export interface RuntimeCatalogEntry {
  kind: RuntimeKind;
  /** Short brand name (proxy form, MCP messages): "Claude". */
  label: string;
  /** Product / CLI name for the agent create + profile pickers:
   *  "Claude Code". Equal to `label` when there's no distinct CLI name. */
  cliLabel: string;
  /**
   * Offerable as an agent runtime in the create / profile pickers. This
   * is a curated PRODUCT flag, NOT "has a driver": cursor has a driver
   * but isn't offered yet. Invariant (asserted host-side, where driver
   * registration is visible): `selectable ⇒ hasDriver(kind)` — we never
   * offer a runtime we can't actually spawn.
   */
  selectable: boolean;
  /**
   * User-facing reasoning-effort options for this runtime, in display
   * order (empty = no Reasoning control). These are the per-runtime
   * allowed values (lowercase tokens) stored + passed to the driver
   * verbatim — the AUTHORITATIVE and SOLE allowlist of what this runtime
   * accepts. The shared `ReasoningEffort` type is only a bounded opaque
   * token (structure, not semantics); it grants no legality. This is the
   * runtime baseline; `reasoningEffortOptionsByModel` may replace it for a
   * verified model. Keeping the allowed set here means a value legal for
   * one runtime/model (e.g. qoder's `none`, Codex's model-specific `ultra`)
   * cannot leak into another runtime or model picker.
   */
  reasoningEffortOptions: readonly ReasoningEffort[];
  /**
   * Full replacement option lists for models whose effort capability differs
   * from the runtime baseline. Default and unlisted models outside approved families use the
   * baseline: free-text model ids are open-ended, so Silverhand must not
   * promise a model-specific effort without an explicit capability entry.
   */
  reasoningEffortOptionsByModel?: Readonly<Record<string, readonly ReasoningEffort[]>>;
  /** Approved model families: matches the exact family or a nonempty `family-*`
   * suffix. Exact model entries take precedence; unrelated names use baseline. */
  reasoningEffortOptionsByModelFamily?: Readonly<Record<string, readonly ReasoningEffort[]>>;
  /** Whether the runtime accepts the persisted fast-mode tri-state. */
  supportsFastMode: boolean;
  /**
   * Profile MCP servers load at spawn for this runtime. Source of truth
   * is the host spawn loader (`mcp-spawn.ts`); this mirrors it and a
   * host-side test asserts the two stay in lockstep.
   */
  supportsProfileMcp: boolean;
  /**
   * Suggested model values for the profile model picker (e.g. qoder's
   * capability tiers). Free-text model ids are ALWAYS also allowed —
   * these are quick-pick suggestions, not a closed enum. `[]` = no
   * suggestions (free-text only).
   */
  modelSuggestions: readonly string[];
  /**
   * Stored model to apply when creating an agent without an explicit
   * `model`. Omitted means preserve the runtime's own CLI default by
   * storing NULL and passing no `--model`. Qoder is the exception today:
   * product default is the `Ultimate` tier while still allowing custom
   * model-id strings later through the profile combobox.
   */
  defaultModel?: string;
}

// Exhaustive by construction: `Record<RuntimeKind, …>` forces an entry
// for every runtime kind, so a new kind can't ship without a catalog row.
const CATALOG: Record<RuntimeKind, RuntimeCatalogEntry> = {
  claude: {
    kind: 'claude',
    label: 'Claude',
    cliLabel: 'Claude Code',
    selectable: true,
    reasoningEffortOptions: [],
    supportsFastMode: false,
    supportsProfileMcp: true,
    modelSuggestions: [],
  },
  codex: {
    kind: 'codex',
    label: 'Codex',
    cliLabel: 'Codex CLI',
    selectable: true,
    reasoningEffortOptions: ['low', 'medium', 'high', 'xhigh'],
    // 5.6 remains exact-model gated. Shiki approved the GPT-6 family for
    // Max/Ultra; its family boundary must not match gpt-60 or proxy aliases.
    reasoningEffortOptionsByModel: {
      'gpt-6.1-sol': ['low', 'medium', 'high', 'xhigh', 'max', 'ultra'],
      'gpt-6.1-terra': ['low', 'medium', 'high', 'xhigh', 'max', 'ultra'],
      'gpt-5.6-sol': ['low', 'medium', 'high', 'xhigh', 'max', 'ultra'],
      'gpt-5.6-terra': ['low', 'medium', 'high', 'xhigh', 'max'],
      'gpt-5.6-luna': ['low', 'medium', 'high', 'xhigh', 'max'],
    },
    reasoningEffortOptionsByModelFamily: {
      'gpt-6': ['low', 'medium', 'high', 'xhigh', 'max', 'ultra'],
    },
    supportsFastMode: true,
    supportsProfileMcp: true,
    // Suggested model ids for the Model combobox. Free text still accepted;
    // these are quick-picks only (no `defaultModel` — codex has no forced
    // create-time default).
    modelSuggestions: [
      'gpt-6.1-sol',
      'gpt-6.1-terra',
      'gpt-6-astra',
      'gpt-5.6-sol',
      'gpt-5.6-terra',
      'gpt-5.6-luna',
    ],
  },
  qoder: {
    kind: 'qoder',
    label: 'Qoder',
    cliLabel: 'Qoder',
    selectable: true,
    // qoder's full allowed set (known from the qodercli binary).
    reasoningEffortOptions: ['none', 'low', 'medium', 'high', 'xhigh', 'max'],
    supportsFastMode: false,
    supportsProfileMcp: true,
    // The five capability tiers from `qodercli --list-models`. Specific
    // model ids (Qwen*/GLM*/DeepSeek*/Kimi*/MiniMax*) go through free
    // text, per shiki: keep the tiers as suggestions, allow custom ids.
    modelSuggestions: ['Auto', 'Ultimate', 'Performance', 'Efficient', 'Lite'],
    defaultModel: 'Ultimate',
  },
  opencode: {
    kind: 'opencode',
    label: 'OpenCode',
    cliLabel: 'OpenCode',
    selectable: true,
    reasoningEffortOptions: [],
    supportsFastMode: false,
    // Profile MCP rides the deep-merged OPENCODE_CONFIG_CONTENT env seam
    // (#task:1405) — the same additive layer claude/codex/qoder get, so
    // opencode reaches parity rather than shipping uniquely weaker.
    supportsProfileMcp: true,
    modelSuggestions: [],
  },
  pi: {
    kind: 'pi',
    label: 'Pi',
    cliLabel: 'Pi',
    selectable: true,
    reasoningEffortOptions: ['off', 'minimal', 'low', 'medium', 'high'],
    supportsFastMode: false,
    // User-installed Pi extensions remain enabled. This flag describes only
    // Silverhand profile-config injection, which needs an extension contract.
    supportsProfileMcp: false,
    modelSuggestions: [],
  },
  grok: {
    kind: 'grok',
    label: 'Grok',
    cliLabel: 'Grok Build',
    selectable: true,
    reasoningEffortOptions: ['low', 'medium', 'high'],
    reasoningEffortOptionsByModel: { 'grok-4.6': ['low', 'medium', 'high', 'xhigh'] },
    supportsFastMode: false,
    supportsProfileMcp: true,
    modelSuggestions: ['grok-4.6', 'grok-4.5'],
  },
  cursor: {
    kind: 'cursor',
    label: 'Cursor',
    cliLabel: 'Cursor',
    selectable: false,
    reasoningEffortOptions: [],
    supportsFastMode: false,
    supportsProfileMcp: false,
    modelSuggestions: [],
  },
  gemini: {
    kind: 'gemini',
    label: 'Gemini',
    cliLabel: 'Gemini',
    selectable: false,
    reasoningEffortOptions: [],
    supportsFastMode: false,
    supportsProfileMcp: false,
    modelSuggestions: [],
  },
};

/**
 * Display order for the pickers and the proxy runtime list: selectable
 * runtimes first (create/profile order), then roadmap runtimes. A test
 * asserts this is a permutation of `RuntimeKindSchema.options` so a new
 * kind can't be omitted from the ordering.
 */
const RUNTIME_ORDER: readonly RuntimeKind[] = [
  'claude',
  'codex',
  'qoder',
  'opencode',
  'pi',
  'grok',
  'cursor',
  'gemini',
];

/** All runtime entries in display order. Proxy form lists every kind
 *  (availability-agnostic); create/profile filter on `selectable`. */
export const RUNTIME_CATALOG: readonly RuntimeCatalogEntry[] = RUNTIME_ORDER.map(
  (kind) => CATALOG[kind],
);

/** All known runtime kinds in display order. */
export const RUNTIME_ORDER_LIST: readonly RuntimeKind[] = RUNTIME_ORDER;

export function runtimeCatalogEntry(kind: RuntimeKind): RuntimeCatalogEntry {
  return CATALOG[kind];
}

/** Short brand label ("Claude"). Falls back to the raw kind for any
 *  value outside the enum (defensive; the type makes this unreachable). */
export function runtimeLabel(kind: RuntimeKind): string {
  return CATALOG[kind]?.label ?? kind;
}

/** Product / CLI label ("Claude Code"). */
export function runtimeCliLabel(kind: RuntimeKind): string {
  return CATALOG[kind]?.cliLabel ?? kind;
}

/** Stored create-time default model, if this runtime has one. */
export function runtimeDefaultModel(kind: RuntimeKind): string | null {
  return CATALOG[kind].defaultModel ?? null;
}

/** True if the runtime exposes a reasoning-effort control. */
export function runtimeSupportsReasoningEffort(kind: RuntimeKind): boolean {
  return CATALOG[kind].reasoningEffortOptions.length > 0;
}

/** Model-aware reasoning options. The runtime baseline is the safe fallback
 * for driver-default, custom, and future model ids whose capability has not
 * been explicitly verified, either individually or as an approved family. */
export function reasoningEffortOptionsForRuntime(
  kind: RuntimeKind,
  model: string | null | undefined,
): readonly ReasoningEffort[] {
  const entry = CATALOG[kind];
  const modelId = model?.trim();
  if (!modelId) return entry.reasoningEffortOptions;
  const exact = entry.reasoningEffortOptionsByModel?.[modelId];
  if (exact) return exact;
  for (const [family, options] of Object.entries(entry.reasoningEffortOptionsByModelFamily ?? {})) {
    if (
      modelId === family ||
      (modelId.startsWith(`${family}-`) && modelId.length > family.length + 1)
    )
      return options;
  }
  return entry.reasoningEffortOptions;
}

/** True when the runtime exposes the fast-mode control. */
export function runtimeSupportsFastMode(kind: RuntimeKind): boolean {
  return CATALOG[kind].supportsFastMode;
}

/**
 * Domain invariant: a stored `reasoning_effort` must be `null` (unset) OR a
 * token this runtime/model pair actually supports (per its catalog options).
 * The UI picker only offers valid values, but this is the AUTHORITATIVE check
 * every write boundary enforces — create/switch (schema superRefine) and
 * update (`normalizeRuntimeTuning`) — so a direct-API call or a runtime switch
 * can't persist e.g. qoder's `none` onto a codex agent (which would then
 * forward an out-of-range token to the codex driver). Keeping the rule here —
 * beside the options it reads — means every write boundary shares one
 * definition instead of hand-copied allowlists.
 */
export function reasoningEffortValidForRuntime(
  kind: RuntimeKind,
  value: ReasoningEffort | null | undefined,
  model?: string | null,
): boolean {
  if (value === null || value === undefined) return true;
  return reasoningEffortOptionsForRuntime(kind, model).includes(value);
}

/** True when `value` is part of the runtime's HISTORICAL baseline — the set
 *  every daemon (old or new) accepts without any capability proof. A
 *  non-baseline value (e.g. Codex `ultra`) is an EXTENSION the backend only
 *  sends when the target host's daemon advertises `runtime.codex.reasoning.v1`;
 *  product validity (which model may use it) is enforced by the static product
 *  table (`reasoningEffortOptionsByModel`). */
export function isBaselineReasoningEffort(kind: RuntimeKind, value: ReasoningEffort): boolean {
  return CATALOG[kind].reasoningEffortOptions.includes(value);
}

/** Generic display label for a reasoning-effort value (title-cased):
 *  `xhigh` → "Xhigh", `none` → "None". Persistence + driver args use
 *  the raw lowercase token; this is display-only. */
export function reasoningEffortLabel(value: ReasoningEffort): string {
  return value.charAt(0).toUpperCase() + value.slice(1);
}
