import { useEffect, useMemo, useRef, useState, type JSX } from "react";
import * as Select from "@radix-ui/react-select";
import { ChevronDownIcon, RefreshIcon } from "@/icons";
import { toast } from "@/lib/toast";
import type {
  AgentDto,
  ReasoningEffort,
  RuntimeAvailabilityState,
  RuntimeKind,
  SwitchAgentRuntimeInput,
  UpdateAgentInput,
} from "@silverhand/shared";
import {
  RUNTIME_CATALOG,
  reasoningEffortLabel,
  reasoningEffortOptionsForRuntime,
  reasoningEffortValidForRuntime,
  runtimeCatalogEntry,
  runtimeCliLabel,
  runtimeSupportsFastMode,
  runtimeSupportsReasoningEffort,
} from "@silverhand/shared";
import { Combobox } from "@/components/ui/combobox";
import { Label } from "@/components/ui/input";
import { cn } from "@/lib/cn";
import { describeApiError } from "@/lib/api-error-message";
import { CancelButton, SaveButton } from "@/components/common/edit-toolbar-buttons";
import { SwitchRuntimeConfirmDialog } from "./switch-runtime-confirm-dialog";
import { useSwitchAgentRuntime, useUpdateAgent } from "../hooks/hooks";
import { useHostAvailabilityMap, useHostRuntimeCatalog } from "@/features/computers/hooks/hooks";
import { isComputerAvailable } from "@/features/computers/model/host-display";
import {
  deriveRuntimePickerMode,
  runtimeChipDisabled,
  runtimeSectionHint,
} from "../model/runtime-detection-ui";
import { t } from "@/lib/i18n";

/**
 * Runtime configuration section — one edit affordance covering all four
 * coupled fields (runtime, model, reasoning_effort, fast_mode).
 *
 * Shiki 2026-07-12: "整个 runtime config 给一个 edit icon" — one pencil
 * enters an edit session over the whole group; leaving the section either
 * saves the full tuple atomically or discards the draft.
 *
 * Save routing:
 *  - runtime changed → `PUT /api/agents/:id/runtime` (transition command,
 *    stops child processes + clears CLI session + resets tuning atomically).
 *  - runtime unchanged, only tuning changed → `PATCH /api/agents/:id` with
 *    the diff. Runtime is `z.never()` in the PATCH schema, so we cannot
 *    lazily route everything through PATCH.
 *
 * Runtime change resets tuning: picking a new target inside edit mode drops
 * the model/reasoning/fast_mode drafts back to the "unset" pre-choice state
 * so the user must positively pick fresh values (matches shiki's "no silent
 * default selection" rule — same discipline as the previous transition
 * dialog).
 */

/** Local draft. Mirrors the UI state and is the input to every pure helper
 *  (canSave / routeSave / buildPatch). Exported for the unit tests. */
export interface RuntimeConfigDraft {
  runtime: RuntimeKind;
  modelChoice: "unset" | "default" | "specify";
  modelValue: string;
  reasoning: "" | "__default__" | ReasoningEffort;
  fastMode: "" | "__default__" | "true" | "false";
}

/** Seed a draft from the persisted agent — used on `enterEdit` and on
 *  Cancel to re-hydrate. Every field maps to a concrete draft position
 *  (never "unset") because the persisted state IS an explicit choice. */
export function draftFromAgent(agent: AgentDto): RuntimeConfigDraft {
  return {
    runtime: agent.runtime,
    modelChoice: agent.model === null ? "default" : "specify",
    modelValue: agent.model ?? "",
    reasoning: reasoningToDraft(agent.runtime, agent.reasoning_effort),
    fastMode: fastModeToDraft(agent.runtime, agent.fast_mode),
  };
}

function reasoningToDraft(
  runtime: RuntimeKind,
  value: ReasoningEffort | null,
): RuntimeConfigDraft["reasoning"] {
  if (!runtimeSupportsReasoningEffort(runtime)) return "";
  return value ?? "__default__";
}

function fastModeToDraft(
  runtime: RuntimeKind,
  value: boolean | null,
): RuntimeConfigDraft["fastMode"] {
  if (!runtimeSupportsFastMode(runtime)) return "";
  if (value === null) return "__default__";
  return value ? "true" : "false";
}

/** Return the draft after a runtime pick. When the target differs from the
 *  initial runtime, tuning is reset to the "unset" position so the user
 *  must reconfigure — this is the load-bearing rule shiki called out
 *  ("runtime change 会清空其他几个值"). When the target equals the initial
 *  runtime, the draft is re-hydrated from the persisted agent so returning
 *  to the original target undoes the tuning reset without losing the user's
 *  starting point.
 *
 *  Takes no current draft on purpose: neither arm reads it — one re-hydrates
 *  from `agent`, the other blanks every tuning field. */
export function draftAfterRuntimeChange(agent: AgentDto, target: RuntimeKind): RuntimeConfigDraft {
  if (target === agent.runtime) return draftFromAgent(agent);
  return {
    runtime: target,
    modelChoice: "unset",
    modelValue: "",
    reasoning: "",
    fastMode: "",
  };
}

/** True iff the draft is different from what's persisted. Save button gates
 *  on this — an unchanged draft cannot be committed. */
export function draftIsDirty(agent: AgentDto, draft: RuntimeConfigDraft): boolean {
  const base = draftFromAgent(agent);
  return (
    draft.runtime !== base.runtime ||
    draft.modelChoice !== base.modelChoice ||
    draft.modelValue.trim() !== base.modelValue.trim() ||
    draft.reasoning !== base.reasoning ||
    draft.fastMode !== base.fastMode
  );
}

/** True iff every visible field is filled (concrete value or explicit "Use
 *  driver default"). Same discipline as the old transition dialog:
 *  the system NEVER picks a default silently.
 *
 *  Reasoning validity uses the static product table (`reasoningEffortValidForRuntime`),
 *  which encodes the model-specific rules (e.g. Codex `ultra` only on sol). */
export function canSaveRuntimeConfig(agent: AgentDto, draft: RuntimeConfigDraft): boolean {
  if (!draftIsDirty(agent, draft)) return false;
  if (draft.modelChoice === "unset") return false;
  if (draft.modelChoice === "specify" && draft.modelValue.trim().length === 0) {
    return false;
  }
  if (runtimeSupportsReasoningEffort(draft.runtime) && draft.reasoning === "") {
    return false;
  }
  const model = draft.modelChoice === "specify" ? draft.modelValue.trim() : null;
  const reasoning =
    draft.reasoning === "" || draft.reasoning === "__default__" ? null : draft.reasoning;
  if (!reasoningEffortValidForRuntime(draft.runtime, reasoning, model)) {
    return false;
  }
  if (runtimeSupportsFastMode(draft.runtime) && draft.fastMode === "") {
    return false;
  }
  return true;
}

/** Convert a valid draft to the wire payload for the runtime transition
 *  endpoint. Fields the target runtime doesn't support are always `null` —
 *  the shared schema rejects any other value on non-supporting runtimes. */
export function buildSwitchPayload(draft: RuntimeConfigDraft): SwitchAgentRuntimeInput {
  const model = draft.modelChoice === "default" ? null : draft.modelValue.trim();
  const reasoning_effort: ReasoningEffort | null = !runtimeSupportsReasoningEffort(draft.runtime)
    ? null
    : draft.reasoning === "__default__" || draft.reasoning === ""
      ? null
      : (draft.reasoning as ReasoningEffort);
  const fast_mode: boolean | null = !runtimeSupportsFastMode(draft.runtime)
    ? null
    : draft.fastMode === "__default__" || draft.fastMode === ""
      ? null
      : draft.fastMode === "true";
  return {
    runtime: draft.runtime,
    model,
    reasoning_effort,
    fast_mode,
  };
}

/** Diff a valid unchanged-runtime draft against the persisted agent and
 *  return only the changed tuning fields. Runtime is deliberately NOT in
 *  the shape (it belongs to the transition command; PATCH schema rejects
 *  it as `z.never()`). */
export function buildTuningPatch(agent: AgentDto, draft: RuntimeConfigDraft): UpdateAgentInput {
  const patch: UpdateAgentInput = {};
  const nextModel = draft.modelChoice === "default" ? null : draft.modelValue.trim();
  if (nextModel !== agent.model) patch.model = nextModel;
  if (runtimeSupportsReasoningEffort(draft.runtime)) {
    const nextReasoning: ReasoningEffort | null =
      draft.reasoning === "__default__" || draft.reasoning === ""
        ? null
        : (draft.reasoning as ReasoningEffort);
    if (nextReasoning !== agent.reasoning_effort) {
      patch.reasoning_effort = nextReasoning;
    }
  }
  if (runtimeSupportsFastMode(draft.runtime)) {
    const nextFastMode: boolean | null =
      draft.fastMode === "__default__" || draft.fastMode === "" ? null : draft.fastMode === "true";
    if (nextFastMode !== agent.fast_mode) patch.fast_mode = nextFastMode;
  }
  return patch;
}

/** Decide which wire endpoint the Save should route to. Runtime change
 *  goes through the dedicated transition command; unchanged-runtime
 *  tuning drift goes through the ordinary PATCH. `'nothing'` means the
 *  draft doesn't differ from the persisted state — Save is a no-op. */
export function routeRuntimeConfigSave(
  agent: AgentDto,
  draft: RuntimeConfigDraft,
):
  | { kind: "switch"; payload: SwitchAgentRuntimeInput }
  | { kind: "patch"; payload: UpdateAgentInput }
  | { kind: "nothing" } {
  if (!draftIsDirty(agent, draft)) return { kind: "nothing" };
  if (draft.runtime !== agent.runtime) {
    return { kind: "switch", payload: buildSwitchPayload(draft) };
  }
  return { kind: "patch", payload: buildTuningPatch(agent, draft) };
}

/** Toast copy for a successful transition, mirroring the partial-success
 *  behavior (warning if any conversation couldn't restart). */
export function describeSwitchToast(
  runtime: RuntimeKind,
  restartFailures: number,
): { tone: "success" | "warning"; message: string } {
  const label = runtimeCliLabel(runtime);
  if (restartFailures > 0) {
    return {
      tone: "warning",
      message: t("agents.switchedPartial", { runtime: label, count: restartFailures }),
    };
  }
  return { tone: "success", message: t("agents.switched", { runtime: label }) };
}

interface RuntimeConfigEditorProps {
  agent: AgentDto;
  /** Editing state, owned by the parent so the section's header slot can
   *  render the pencil trigger next to the section title. */
  editing: boolean;
  onExitEdit: () => void;
}

export function RuntimeConfigEditor({
  agent,
  editing,
  onExitEdit,
}: RuntimeConfigEditorProps): JSX.Element {
  const [draft, setDraft] = useState<RuntimeConfigDraft>(() => draftFromAgent(agent));
  const [pendingSwitch, setPendingSwitch] = useState<SwitchAgentRuntimeInput | null>(null);
  const switchMut = useSwitchAgentRuntime(agent.id);
  const patchMut = useUpdateAgent(agent.id);
  const pending = switchMut.isPending || patchMut.isPending;
  // Owned by the parent so the confirm dialog can restore focus here on
  // close — Radix Dialog has no `DialogTrigger` in the controlled open
  // pattern and would otherwise drop focus to `<body>`, breaking the
  // subsequent-Escape → cancel-edit path.
  const runtimeTriggerRef = useRef<HTMLButtonElement | null>(null);

  // Re-hydrate the draft whenever a fresh agent lands (successful commit,
  // realtime update, or agent switch). Only meaningful when NOT editing —
  // an in-flight edit shouldn't be clobbered by an unrelated invalidation.
  useEffect(() => {
    if (!editing) setDraft(draftFromAgent(agent));
  }, [agent, editing]);

  // Enter-edit: pull focus into the Runtime Select trigger so the parent
  // capture handler receives Escape (otherwise the section pencil retains
  // focus and Esc fires on `<body>`).
  useEffect(() => {
    if (editing) runtimeTriggerRef.current?.focus({ preventScroll: true });
  }, [editing]);

  // Reset in-flight mutation state whenever the parent enters edit. The
  // draft itself is already in sync via the readonly-sync effect above
  // (state initialized from `agent`, re-hydrated on every non-editing
  // render), so DO NOT re-seed here — a second setDraft after `editing`
  // flips true renders a first paint against the pre-seed draft and only
  // the second frame reflects `agent`, visible as a one-frame flash on
  // pencil click (Cindy round-8 axis B).
  useEffect(() => {
    if (editing) {
      switchMut.reset();
      patchMut.reset();
      setPendingSwitch(null);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [editing]);

  const targets = useMemo(() => RUNTIME_CATALOG.filter((entry) => entry.selectable), []);

  const catalogEntry = runtimeCatalogEntry(draft.runtime);

  // The host runtime catalog + availability are fetched HERE (the parent that
  // owns Save/canSave) and passed to EditForm for the runtime install display.
  // The probe LIFECYCLE is edit-only: a readonly profile view must never fetch
  // — both are gated on `editing` (empty `hostIds` + `enabled: editing`), so
  // opening a profile costs nothing; React Query dedups the shared query key.
  const hostIds = useMemo(() => (editing ? [agent.host_id] : []), [editing, agent.host_id]);
  const { availabilityById, allSettled } = useHostAvailabilityMap(hostIds);
  const hostAvailable = isComputerAvailable(availabilityById.get(agent.host_id));
  const runtimeCatalog = useHostRuntimeCatalog(agent.host_id, {
    enabled: editing && allSettled && hostAvailable,
  });

  function cancel() {
    if (pending) return;
    setDraft(draftFromAgent(agent));
    switchMut.reset();
    patchMut.reset();
    setPendingSwitch(null);
    onExitEdit();
  }
  async function runSwitch(payload: SwitchAgentRuntimeInput) {
    try {
      const result = await switchMut.mutateAsync(payload);
      const t = describeSwitchToast(payload.runtime, result.restart_failures);
      if (t.tone === "warning") toast.warning(t.message);
      else toast.success(t.message);
      setPendingSwitch(null);
      onExitEdit();
    } catch {
      // Keep `pendingSwitch` set so the dialog stays open on failure; the
      // error surfaces inline inside the dialog for retry or Cancel.
    }
  }
  async function onSave() {
    const route = routeRuntimeConfigSave(agent, draft);
    if (route.kind === "nothing") {
      onExitEdit();
      return;
    }
    if (route.kind === "switch") {
      setPendingSwitch(route.payload);
      return;
    }
    try {
      await patchMut.mutateAsync(route.payload);
      onExitEdit();
    } catch {
      // Error surfaces inline via `patchMut.error` below.
    }
  }

  const switchErrorMessage = switchMut.error
    ? describeApiError(switchMut.error, {
        backendUnreachableMessage: t("agents.switchFailedCannotReach"),
        fallbackMessage: t("agents.failedToSwitchRuntime"),
        actionLabel: t("agents.switchRuntime"),
      })
    : null;

  return (
    <div
      className="flex flex-col gap-3"
      onKeyDownCapture={(e) => {
        // Capture Escape before Radix Select triggers consume it. Skip
        // while the confirm dialog is open — Radix Dialog owns Escape
        // there, and canceling the whole edit would drop the draft.
        if (editing && !pending && pendingSwitch === null && e.key === "Escape") {
          e.preventDefault();
          e.stopPropagation();
          cancel();
        }
      }}
    >
      {editing ? (
        <EditForm
          agent={agent}
          draft={draft}
          onDraft={setDraft}
          catalogEntry={catalogEntry}
          targets={targets}
          triggerRef={runtimeTriggerRef}
          allSettled={allSettled}
          hostAvailable={hostAvailable}
          runtimeCatalog={runtimeCatalog}
        />
      ) : (
        <ReadOnlyGrid agent={agent} />
      )}

      {editing && patchMut.error ? (
        <div className="text-xs text-error">
          {describeApiError(patchMut.error, {
            backendUnreachableMessage: t("agents.saveFailedCannotReach"),
            fallbackMessage: t("agents.failedToSaveRuntime"),
            actionLabel: t("common.save"),
          })}
        </div>
      ) : null}

      {editing ? (
        <div className="flex items-center justify-end gap-1.5">
          <CancelButton onClick={cancel} disabled={pending} />
          <SaveButton
            onClick={() => void onSave()}
            disabled={!canSaveRuntimeConfig(agent, draft)}
            pending={pending}
          />
        </div>
      ) : null}

      <SwitchRuntimeConfirmDialog
        open={pendingSwitch !== null}
        onOpenChange={(next) => {
          if (!next && !pending) {
            setPendingSwitch(null);
            switchMut.reset();
          }
        }}
        targetRuntime={pendingSwitch?.runtime ?? draft.runtime}
        pending={pending}
        errorMessage={switchErrorMessage}
        returnFocusRef={runtimeTriggerRef}
        onConfirm={() => {
          if (pendingSwitch) void runSwitch(pendingSwitch);
        }}
      />
    </div>
  );
}

/** Shared by the summary and the edit form so editing keeps the same columns. */
const RUNTIME_GRID_CLASS = "grid grid-cols-[repeat(auto-fit,minmax(9rem,1fr))] gap-x-6 gap-y-3";

/** Read-only presentation of the current runtime + tuning tuple. */
export function ReadOnlyGrid({
  agent,
}: {
  agent: Pick<AgentDto, "runtime" | "model" | "reasoning_effort" | "fast_mode">;
}): JSX.Element {
  const supportsReasoning = runtimeSupportsReasoningEffort(agent.runtime);
  const supportsFastMode = runtimeSupportsFastMode(agent.runtime);
  return (
    <div data-runtime-summary className={RUNTIME_GRID_CLASS}>
      <ReadOnlyRow label={t("agents.runtime")} value={runtimeCliLabel(agent.runtime)} />
      <ReadOnlyRow
        label={t("agents.model")}
        value={agent.model}
        placeholder={t("agents.driverDefault")}
      />
      {supportsReasoning ? (
        <ReadOnlyRow
          label={t("agents.reasoning")}
          value={agent.reasoning_effort ? reasoningEffortLabel(agent.reasoning_effort) : null}
          placeholder={t("agents.driverDefault")}
        />
      ) : null}
      {supportsFastMode ? (
        <ReadOnlyRow
          label={t("agents.fastMode")}
          value={
            agent.fast_mode === null || agent.fast_mode === undefined
              ? null
              : agent.fast_mode
                ? "Enabled"
                : "Disabled"
          }
          placeholder={t("agents.driverDefault")}
        />
      ) : null}
    </div>
  );
}

function ReadOnlyRow({
  label,
  value,
  placeholder,
}: {
  label: string;
  value: string | null | undefined;
  placeholder?: string;
}): JSX.Element {
  const displayed = value && value.length > 0 ? value : null;
  return (
    <div className="flex min-w-0 flex-col gap-0.5">
      <span className="text-xs text-text-muted">{label}</span>
      <span
        className={cn(
          "text-sm wrap-anywhere",
          displayed ? "text-text-primary" : "text-text-muted italic",
        )}
      >
        {displayed ?? placeholder ?? "—"}
      </span>
    </div>
  );
}

/** Editable presentation. Every visible field is required (concrete value
 *  or explicit "Use driver default"); a runtime change resets the tuning
 *  triple so the user positively re-selects each. */
function EditForm({
  agent,
  draft,
  onDraft,
  catalogEntry,
  targets,
  triggerRef,
  allSettled,
  hostAvailable,
  runtimeCatalog,
}: {
  agent: AgentDto;
  draft: RuntimeConfigDraft;
  onDraft: (next: RuntimeConfigDraft) => void;
  catalogEntry: ReturnType<typeof runtimeCatalogEntry>;
  targets: readonly { kind: RuntimeKind }[];
  triggerRef: React.Ref<HTMLButtonElement>;
  // Availability + runtime catalog are lifted to the parent (which owns
  // Save/canSave) so the picker and the Save gate read ONE source.
  allSettled: boolean;
  hostAvailable: boolean;
  runtimeCatalog: ReturnType<typeof useHostRuntimeCatalog>;
}): JSX.Element {
  const supportsReasoning = runtimeSupportsReasoningEffort(draft.runtime);
  const supportsFastMode = runtimeSupportsFastMode(draft.runtime);

  const runtimeStateByKind = useMemo(() => {
    const map = new Map<RuntimeKind, RuntimeAvailabilityState>();
    for (const info of runtimeCatalog.data?.runtimes ?? []) {
      map.set(info.kind, info.state);
    }
    return map;
  }, [runtimeCatalog.data]);
  // Reasoning options come from the static product table (model-aware): the
  // per-model allowlist encodes extensions like Codex Ultra (sol only). The
  // open wire token + daemon capability keep this from bumping the protocol.
  const reasoningModel = draft.modelChoice === "specify" ? draft.modelValue : null;
  const reasoningOptions = reasoningEffortOptionsForRuntime(draft.runtime, reasoningModel);
  // Re-add an already-selected value when it isn't in the current options — a
  // legal persisted value must not vanish from the picker (e.g. after a model
  // change the commit path clears it into a positive re-pick; this is the
  // load-time guard so the stored value stays visible until then).
  const reasoningDisplayOptions =
    draft.reasoning !== "" &&
    draft.reasoning !== "__default__" &&
    !reasoningOptions.includes(draft.reasoning)
      ? [...reasoningOptions, draft.reasoning]
      : reasoningOptions;
  const mode = deriveRuntimePickerMode({
    // Agent always has a host — probing only skips when the host itself
    // is offline / probe errored (mode → 'offline' / 'probe-failed').
    hasHost: true,
    isHostAvailabilityLoading: !allSettled,
    hostAvailable,
    isLoading: runtimeCatalog.isLoading,
    isError: runtimeCatalog.isError,
  });
  const sectionHint = runtimeSectionHint(mode);
  const detecting = !allSettled || runtimeCatalog.isLoading || runtimeCatalog.isReprobing;

  return (
    <div className="flex flex-col gap-3">
      <div className={RUNTIME_GRID_CLASS}>
        <div className="flex min-w-0 flex-col gap-1.5">
          {/* All four label rows in this grid are fixed h-5 (see Model /
           * Reasoning / Fast mode wrappers below). Recheck lives INSIDE
           * that height: `size-5 p-0` = 20×20 button, icon `size-3.5` =
           * 14px, box never exceeds row height so its show/hide can't
           * move the Select trigger top (Cindy round-8 #1). shiki
           * measured the pre-fix version's real culprit: Recheck's
           * box was ~24px (14px icon + p-1 border), not the glyph. */}
          <div className="flex h-5 items-center justify-between gap-2">
            <div className="flex items-center gap-2">
              <Label htmlFor="rc-runtime" required>
                {t("agents.runtime")}
              </Label>
              {detecting ? (
                <span className="text-xs font-normal text-text-muted">{t("agents.detecting")}</span>
              ) : null}
            </div>
            {hostAvailable ? (
              <button
                type="button"
                onClick={() => void runtimeCatalog.reprobe()}
                disabled={detecting}
                aria-label={t("agents.recheckInstalledRuntimes")}
                title={t("agents.recheck")}
                className="inline-flex size-5 items-center justify-center p-0 text-text-muted transition-colors hover:text-text-primary disabled:cursor-default disabled:opacity-60"
              >
                <RefreshIcon className={cn("size-3.5", detecting && "animate-spin")} />
              </button>
            ) : null}
          </div>
          <RuntimeSelect
            id="rc-runtime"
            targets={targets}
            value={draft.runtime}
            mode={mode}
            runtimeStateByKind={runtimeStateByKind}
            triggerRef={triggerRef}
            onChange={(next) => onDraft(draftAfterRuntimeChange(agent, next))}
          />
          {sectionHint ? <p className="text-xs text-text-muted">{sectionHint}</p> : null}
        </div>

        <div className="flex min-w-0 flex-col gap-1.5">
          <div className="flex h-5 items-center">
            <Label required>{t("agents.model")}</Label>
          </div>
          <ModelPicker
            id="rc-model"
            draft={draft}
            suggestions={catalogEntry.modelSuggestions}
            onDraft={onDraft}
          />
        </div>

        {supportsReasoning ? (
          <div className="flex min-w-0 flex-col gap-1.5">
            <div className="flex h-5 items-center">
              <Label htmlFor="rc-reasoning" required>
                {t("agents.reasoning")}
              </Label>
            </div>
            <ChoicePickerSelect
              id="rc-reasoning"
              value={draft.reasoning}
              placeholder={t("agents.choose")}
              options={[
                { value: "__default__", label: t("agents.useDriverDefault") },
                ...reasoningDisplayOptions.map((v) => ({
                  value: v,
                  label: reasoningEffortLabel(v),
                })),
              ]}
              onChange={(v) =>
                onDraft({
                  ...draft,
                  reasoning: v as RuntimeConfigDraft["reasoning"],
                })
              }
            />
          </div>
        ) : null}

        {supportsFastMode ? (
          <div className="flex min-w-0 flex-col gap-1.5">
            <div className="flex h-5 items-center">
              <Label htmlFor="rc-fastmode" required>
                {t("agents.fastMode")}
              </Label>
            </div>
            <ChoicePickerSelect
              id="rc-fastmode"
              value={draft.fastMode}
              placeholder={t("agents.choose")}
              options={[
                { value: "__default__", label: t("agents.useCodexDefault") },
                { value: "true", label: t("agents.enabled") },
                { value: "false", label: t("agents.disabled") },
              ]}
              onChange={(v) =>
                onDraft({
                  ...draft,
                  fastMode: v as RuntimeConfigDraft["fastMode"],
                })
              }
            />
          </div>
        ) : null}
      </div>
    </div>
  );
}

function RuntimeSelect({
  id,
  targets,
  value,
  onChange,
  mode,
  runtimeStateByKind,
  triggerRef,
}: {
  id: string;
  targets: readonly { kind: RuntimeKind }[];
  value: RuntimeKind;
  onChange: (runtime: RuntimeKind) => void;
  mode: ReturnType<typeof deriveRuntimePickerMode>;
  runtimeStateByKind: Map<RuntimeKind, RuntimeAvailabilityState>;
  triggerRef?: React.Ref<HTMLButtonElement>;
}): JSX.Element {
  return (
    <Select.Root value={value} onValueChange={(v) => onChange(v as RuntimeKind)}>
      <Select.Trigger
        id={id}
        ref={triggerRef}
        data-panel-control="select-trigger"
        className={triggerClass()}
      >
        <span className="min-w-0 truncate">
          <Select.Value />
        </span>
        <Select.Icon>
          <ChevronDownIcon className="size-3.5 text-text-muted" />
        </Select.Icon>
      </Select.Trigger>
      <Select.Portal>
        <Select.Content
          position="popper"
          sideOffset={4}
          data-panel-control="select-content"
          className={contentClass()}
        >
          <Select.Viewport>
            {targets.map((target) => {
              const state = runtimeStateByKind.get(target.kind);
              // Disabled ONLY on `detected` + `absent` — a probe gap
              // never traps a runtime the user actually has installed.
              // The final gate is the backend transition; here we just
              // reflect the deterministic negative into a disabled item
              // + a "not installed" trailing marker, matching the New
              // Agent chip picker's semantics.
              const absent = runtimeChipDisabled(mode, state);
              const title =
                mode !== "detected"
                  ? undefined
                  : state === "absent"
                    ? t("agents.notDetectedOnThis")
                    : state === "present_unverified"
                      ? t("agents.installedVersionUnverified")
                      : state === "unknown"
                        ? t("agents.couldntVerifyYouCan")
                        : undefined;
              return (
                <Select.Item
                  key={target.kind}
                  value={target.kind}
                  disabled={absent}
                  title={title}
                  className={itemClass()}
                >
                  <Select.ItemText>{runtimeCliLabel(target.kind)}</Select.ItemText>
                  {absent ? (
                    <span className="ml-auto text-xs text-text-muted">
                      {t("agents.notInstalled")}
                    </span>
                  ) : null}
                </Select.Item>
              );
            })}
          </Select.Viewport>
        </Select.Content>
      </Select.Portal>
    </Select.Root>
  );
}

function ChoicePickerSelect({
  id,
  value,
  placeholder,
  options,
  onChange,
}: {
  id: string;
  value: string;
  placeholder: string;
  options: { value: string; label: string }[];
  onChange: (v: string) => void;
}): JSX.Element {
  return (
    <Select.Root value={value} onValueChange={onChange}>
      <Select.Trigger id={id} data-panel-control="select-trigger" className={triggerClass()}>
        <span className="min-w-0 truncate">
          <Select.Value placeholder={placeholder} />
        </span>
        <Select.Icon>
          <ChevronDownIcon className="size-3.5 text-text-muted" />
        </Select.Icon>
      </Select.Trigger>
      <Select.Portal>
        <Select.Content
          position="popper"
          sideOffset={4}
          data-panel-control="select-content"
          className={contentClass()}
        >
          <Select.Viewport>
            {options.map((opt) => (
              <Select.Item key={opt.value} value={opt.value} className={itemClass()}>
                <Select.ItemText>{opt.label}</Select.ItemText>
              </Select.Item>
            ))}
          </Select.Viewport>
        </Select.Content>
      </Select.Portal>
    </Select.Root>
  );
}

/**
 * Unified model picker (shiki 2026-07-12 dogfood round-2 #4): a single
 * Combobox — the input itself accepts free-text model ids, the dropdown
 * lists "Use driver default" as the first item followed by known model
 * suggestions. One control, one draft state, no hidden second flow.
 *
 * Draft mapping (still `modelChoice + modelValue` — pure helper surface
 * unchanged):
 *  - `unset`   → input empty, placeholder "Choose or type a model…"
 *                (post-runtime-change forced re-pick)
 *  - `default` → input empty, placeholder "Using driver default"
 *                (dropdown first item is the affordance to enter this state)
 *  - `specify` → input shows the concrete model id (typed or picked)
 *
 * Commit routing:
 *  - Pick "Use driver default" from dropdown  → modelChoice='default'
 *  - Pick a suggestion or type + Enter/blur   → modelChoice='specify'
 *  - Clear the input                          → modelChoice='unset'
 */
const modelDefaultLabel = () => t("agents.useDriverDefault");

function ModelPicker({
  id,
  draft,
  suggestions,
  onDraft,
}: {
  id: string;
  draft: RuntimeConfigDraft;
  suggestions: readonly string[];
  onDraft: (next: RuntimeConfigDraft) => void;
}): JSX.Element {
  const inputValue = draft.modelChoice === "specify" ? draft.modelValue : "";
  const placeholder =
    draft.modelChoice === "default" ? t("agents.usingDriverDefault") : t("agents.chooseOrTypeA");
  const items = [modelDefaultLabel(), ...suggestions];
  return (
    <Combobox
      id={id}
      ariaLabel={t("agents.model")}
      value={inputValue}
      placeholder={placeholder}
      suggestions={items}
      onChange={(next) => {
        // Keystrokes go straight into `specify`. When the user picks the
        // driver-default item from the dropdown, `pick()` fires onChange
        // with the label first — skip it here and let onCommit handle the
        // sentinel below (avoids a transient "specify + modelValue='Use
        // driver default'" flicker).
        if (next === modelDefaultLabel()) return;
        onDraft({ ...draft, modelChoice: "specify", modelValue: next });
      }}
      onCommit={(next) => onDraft(modelDraftAfterCommit(draft, next))}
      // Align model input surface with the sibling Runtime / Reasoning triggers
      // (both are `bg-bg-base` via `selectTriggerClass()`). Shared Combobox
      // defaults to `bg-bg-floating`; we override at THIS consumer only —
      // shiki 2026-07-12 dogfood round-6 + Cindy scope confirm.
      className="h-8 bg-bg-base"
    />
  );
}

/** Pure Combobox-commit → next-draft mapping. Exported for tests so the
 *  three commit outcomes (default sentinel / empty clears to unset / any
 *  other value goes to specify) are independently verifiable.
 *
 *  Empty commit is dispatched by the current `modelChoice`: when the user
 *  is already sitting on `default` the input value is empty by design
 *  (placeholder = "Using driver default"), so a bare focus→blur must NOT
 *  demote `default` to `unset` (Cindy #1472 round-3 HIGH — Combobox
 *  onBlur always fires onCommit(value), and `value` for a default draft
 *  is `''`; without this guard the user's positive choice is silently
 *  overwritten). Only `specify` collapsing to empty means the user
 *  cleared their typed value — that is a real transition to unset. */
export function modelDraftAfterCommit(draft: RuntimeConfigDraft, next: string): RuntimeConfigDraft {
  if (next === modelDefaultLabel()) {
    return clearIncompatibleReasoning({
      ...draft,
      modelChoice: "default",
      modelValue: "",
    });
  }
  const trimmed = next.trim();
  if (trimmed.length === 0) {
    // Bare empty commit: preserve `default` (its input value is empty by
    // design); collapse `specify` to `unset` (user cleared what they
    // typed); leave `unset` unchanged.
    if (draft.modelChoice === "default") return draft;
    return clearIncompatibleReasoning({
      ...draft,
      modelChoice: "unset",
      modelValue: "",
    });
  }
  return clearIncompatibleReasoning({
    ...draft,
    modelChoice: "specify",
    modelValue: trimmed,
  });
}

/** Clear an effort the newly-chosen model can no longer support per the static
 *  product table (e.g. switching off a sol model clears Ultra), forcing a
 *  positive re-pick instead of leaving a stale selection Save would reject. A
 *  baseline value stays valid for any model. */
function clearIncompatibleReasoning(draft: RuntimeConfigDraft): RuntimeConfigDraft {
  if (!runtimeSupportsReasoningEffort(draft.runtime)) return draft;
  if (draft.reasoning === "" || draft.reasoning === "__default__") return draft;
  const model = draft.modelChoice === "specify" ? draft.modelValue.trim() : null;
  if (reasoningEffortValidForRuntime(draft.runtime, draft.reasoning, model)) {
    return draft;
  }
  // Force a positive re-pick instead of silently downgrading (Ultra→…) when the
  // model changes to one the product table excludes.
  return { ...draft, reasoning: "" };
}

function triggerClass(): string {
  return "flex h-8 w-full items-center justify-between gap-2 rounded-[var(--control-radius)] border-[length:var(--control-border-width)] border-border bg-bg-base px-2.5 text-sm text-text-primary outline-none transition-colors hover:border-[color:var(--border-control-hover)] data-[state=open]:border-border-focus focus-visible:border-border-focus data-[placeholder]:text-text-muted";
}

function contentClass(): string {
  return "z-[var(--z-in-dialog)] min-w-[var(--radix-select-trigger-width)] overflow-hidden rounded-[var(--control-radius)] border-[length:var(--control-border-width)] border-border bg-bg-floating p-1 shadow-popover";
}

function itemClass(): string {
  return "flex cursor-default items-center gap-2 rounded-sm px-2 py-1.5 text-sm text-text-primary outline-none data-[highlighted]:bg-bg-hover data-[disabled]:cursor-not-allowed data-[disabled]:text-text-muted data-[disabled]:data-[highlighted]:bg-transparent";
}
