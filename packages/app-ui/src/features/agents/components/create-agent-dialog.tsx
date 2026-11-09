import { useCallback, useEffect, useMemo, useState, type FormEvent } from "react";
import { RefreshIcon } from "@/icons";
import type {
  AgentSessionScope,
  CreateAgentInput,
  RuntimeAvailabilityState,
  RuntimeKind,
} from "@silverhand/shared";
import { RUNTIME_CATALOG, runtimeCatalogEntry, runtimeDefaultModel } from "@silverhand/shared";
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Combobox } from "@/components/ui/combobox";
import { Input, Label } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/cn";
import { describeApiError } from "@/lib/api-error-message";
import {
  NameAvailabilityHint,
  NameCheckingIcon,
  useNameAvailability,
} from "@/features/identity/use-name-availability";
import {
  useHostAvailabilityMap,
  useHostRuntimeCatalog,
  useHostsQuery,
} from "@/features/computers/hooks/hooks";
import { isComputerAvailable } from "@/features/computers/model/host-display";
import { ComputerPicker } from "@/features/computers/components/computer-picker";
import { useCreateAgent, useGiveAgentDefaultAvatar } from "../hooks/hooks";
import {
  deriveRuntimePickerMode,
  runtimeChipDisabled,
  runtimeSectionHint,
} from "../model/runtime-detection-ui";
import { t } from "@/lib/i18n";

// The picker lists only offerable runtimes (`selectable`) — shiki: 没支持的
// 直接不展示 (no roadmap/"coming soon" rows in a focused create task). Which
// of these is actually installed on the chosen computer is detected per host
// at runtime (see the runtime-catalog probe below), never baked in here.
const SELECTABLE_RUNTIMES = RUNTIME_CATALOG.filter((entry) => entry.selectable);

const CHIP_BASE =
  "rounded-[var(--control-radius)] border-[length:var(--control-border-width)] px-3 py-1.5 text-xs transition-colors ";

function chipClass(isSelected: boolean, isDisabled = false): string {
  if (isDisabled) {
    return CHIP_BASE + "cursor-not-allowed border-border-subtle text-text-muted opacity-50";
  }
  return (
    CHIP_BASE +
    (isSelected
      ? "border-text-primary bg-[var(--bg-row-selected)] text-text-on-selected"
      : "border-border-subtle text-text-primary hover:border-border")
  );
}

interface CreateAgentDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onCreated?: (id: string) => void;
  /** Opened from a computer's page: that computer is the default whenever it
   *  is online, ahead of the sole-available rule. */
  preferredHostId?: string;
}

/** The only two runtime-detection facts the parent form needs from the
 *  self-contained <RuntimePicker/>: whether to disable Create (nothing
 *  installed) and the submit backstop (the picked runtime is absent). The
 *  probe, its states, and the Recheck churn all stay inside the child. */
interface RuntimeGate {
  allAbsent: boolean;
  selectedAbsent: boolean;
}

const EMPTY_RUNTIME_GATE: RuntimeGate = {
  allAbsent: false,
  selectedAbsent: false,
};

export interface CreateAgentSubmitDraft {
  name: string;
  runtime: RuntimeKind;
  model: string;
  hostId: string | null;
  sessionScope: AgentSessionScope;
}

/** Canonical product submit shape. Conversation scope remains the wire
 * default for old clients; only a positive Agent-scoped choice widens the
 * payload. This function is the path the real form submits through, so tests
 * can exercise both branches without a second form-only mirror. */
export function createAgentInputFromDraft(draft: CreateAgentSubmitDraft): CreateAgentInput {
  const input: CreateAgentInput = {
    name: draft.name.trim(),
    runtime: draft.runtime,
  };
  const trimmedModel = draft.model.trim();
  if (trimmedModel) input.model = trimmedModel;
  if (draft.hostId) input.host_id = draft.hostId;
  if (draft.sessionScope === "agent") input.session_scope = "agent";
  return input;
}

export function CreateAgentDialog({
  open,
  onOpenChange,
  onCreated,
  preferredHostId,
}: CreateAgentDialogProps): JSX.Element {
  const [name, setName] = useState("");
  const [runtime, setRuntime] = useState<RuntimeKind>("claude");
  const [sessionScope, setSessionScope] = useState<AgentSessionScope>("agent");
  // Model tracks the selected runtime's catalog default (qoder → `Ultimate`,
  // others → blank = the runtime's own CLI default). Switching runtime resets
  // it to that runtime's default so the field always reflects what will be
  // stored (the backend applies the same default, but showing it here keeps
  // create WYSIWYG). Any suggested tier or custom model-id is accepted.
  const [model, setModel] = useState(runtimeDefaultModel("claude") ?? "");
  const [selectedHostId, setSelectedHostId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const createAgent = useCreateAgent();
  const giveDefaultAvatar = useGiveAgentDefaultAvatar();
  // onBlur name validation — format (shared ParticipantNameSchema) + uniqueness
  // (agents ∪ users, server probe). See features/identity/use-name-availability.
  const nameCheck = useNameAvailability("agent", name);

  const modelSuggestions = runtimeCatalogEntry(runtime).modelSuggestions;

  // Switching runtime resets the model to the new runtime's default. A model-id
  // is runtime-specific (qoder tiers vs a codex/claude id), so carrying a stale
  // value across a runtime switch would be meaningless — mirror the backend's
  // per-runtime normalization at the form. `useCallback` keeps a stable
  // identity so passing it to <RuntimePicker/> doesn't churn that child's
  // effects.
  const selectRuntime = useCallback((next: RuntimeKind): void => {
    setRuntime(next);
    setModel(runtimeDefaultModel(next) ?? "");
    setError(null);
  }, []);

  const hostsQuery = useHostsQuery();

  // Placement candidates come straight from the backend's `placeable`
  // predicate — a single, kind-independent eligibility source (doc §5.4). An
  // online, finalized user-managed (daemon) computer is just as valid a target
  // as the local one, so we never re-derive eligibility from `kind` here.
  const placeableHosts = useMemo(
    () => (hostsQuery.data?.hosts ?? []).filter((host) => host.placeable),
    [hostsQuery.data],
  );
  const placeableHostIds = useMemo(() => placeableHosts.map((host) => host.id), [placeableHosts]);

  // Second placement axis: a `placeable` computer can only actually run an
  // agent if its daemon is online right now. We probe live availability per
  // candidate; offline/unreachable ones stay visible but disabled in the
  // picker, and only `available` computers count toward the default and the
  // submit check (shiki #task:1359).
  const { availabilityById, allSettled } = useHostAvailabilityMap(placeableHostIds);
  const availableHosts = useMemo(
    () => placeableHosts.filter((host) => isComputerAvailable(availabilityById.get(host.id))),
    [placeableHosts, availabilityById],
  );

  // The computer picker is always present (0 available → nothing to pick,
  // 1 → preselected, several → no default so the user must choose).
  // `defaultHostId` seeds the selection; `selectedHostId` is the single source
  // of the chosen host. Gate the default on `allSettled` (Cindy P1): while
  // probes resolve one by one, a transient single `available` must NOT
  // auto-select, or two online computers could still end up with a default.
  const defaultHostId =
    preferredHostId && availableHosts.some((host) => host.id === preferredHostId)
      ? preferredHostId
      : allSettled && availableHosts.length === 1
        ? (availableHosts[0]?.id ?? null)
        : null;

  // Preselect the sole available host when the picker opens; clear on close.
  // If a previously-chosen host drops offline while the dialog is open, fall
  // back to the default rather than keeping a now-unpickable selection. `prev`
  // otherwise preserves an explicit user choice.
  useEffect(() => {
    if (!open) {
      setSelectedHostId(null);
      setSessionScope("agent");
      setRuntimeGate(EMPTY_RUNTIME_GATE);
      return;
    }
    const availableIds = new Set(availableHosts.map((host) => host.id));
    setSelectedHostId((prev) => (prev && availableIds.has(prev) ? prev : defaultHostId));
  }, [open, defaultHostId, availableHosts]);

  // The picker is the single source of the chosen host; when no placeable
  // computer exists `selectedHostId` stays null and the backend resolves.
  const effectiveHostId = selectedHostId;

  // Runtime detection is host-local and lives entirely inside <RuntimePicker/>
  // below: it probes the selected computer and reports back only the two gating
  // facts the rest of the form needs (`allAbsent` for the Create button,
  // `selectedAbsent` for the submit backstop). Keeping the probe — and its
  // Recheck / isReprobing churn — inside that child means a Recheck re-renders
  // just the runtime chips, never the whole dialog (shiki: 只刷新 runtime 那些
  //值的状态).
  const selectedHostAvailable = selectedHostId
    ? isComputerAvailable(availabilityById.get(selectedHostId))
    : false;
  const [runtimeGate, setRuntimeGate] = useState<RuntimeGate>(EMPTY_RUNTIME_GATE);

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault();
    setError(null);
    if (!name.trim()) {
      setError(t("agentPage.pleaseEnterAnAgent"));
      return;
    }
    // Final format pass — arms the inline hint even on a field never blurred.
    // Uniqueness is left to create (the authoritative backstop).
    if (!nameCheck.formatValid) {
      nameCheck.markTouched();
      return;
    }
    if (placeableHosts.length > 0) {
      if (!allSettled) {
        // Probes still landing — the Create button is disabled meanwhile, but
        // guard here too so a stale click can't fall through to a misleading
        // "no computer online".
        setError(t("agentPage.checkingWhichComputersAre"));
        return;
      }
      if (availableHosts.length === 0) {
        setError(t("agentPage.noComputerIsOnline"));
        return;
      }
      if (!selectedHostId || !availableHosts.some((h) => h.id === selectedHostId)) {
        setError(t("agentPage.pleaseChooseAnOnline"));
        return;
      }
    }
    // Fail-fast on a genuinely-absent runtime (fresh probe of an available
    // host only — never on a probe gap). <RuntimePicker/>'s auto-fallback
    // normally moves selection off an absent runtime; this guards the
    // all-absent case and any race before that effect runs.
    if (runtimeGate.selectedAbsent) {
      setError(t("agentPage.thatRuntimeIsNot"));
      return;
    }
    try {
      const input = createAgentInputFromDraft({
        name,
        runtime,
        model,
        hostId: effectiveHostId,
        sessionScope,
      });
      const res = await createAgent.mutateAsync(input);
      giveDefaultAvatar(res.agent.id);
      onCreated?.(res.agent.id);
      setName("");
      setRuntime("claude");
      setSessionScope("agent");
      setModel(runtimeDefaultModel("claude") ?? "");
      setSelectedHostId(null);
      setRuntimeGate(EMPTY_RUNTIME_GATE);
      onOpenChange(false);
    } catch (err) {
      setError(friendlyCreateError(err));
    }
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent data-create-agent-dialog aria-describedby={undefined}>
        <DialogHeader>
          <DialogTitle>{t("agentPage.newAgent2")}</DialogTitle>
        </DialogHeader>

        <form onSubmit={handleSubmit} className="flex flex-col gap-3">
          <div className="flex flex-col gap-1.5">
            <Label htmlFor="ag-host" required className="font-semibold">
              {t("agentPage.computer")}
            </Label>
            <ComputerPicker
              id="ag-host"
              hosts={placeableHosts}
              availabilityById={availabilityById}
              value={selectedHostId}
              onChange={(id) => {
                setSelectedHostId(id);
                setError(null);
              }}
              ariaLabel={t("agentPage.computer")}
            />
          </div>

          <div className="flex flex-col gap-1.5">
            <Label htmlFor="ag-name" required className="font-semibold">
              {t("agentPage.name")}
            </Label>
            <div className="relative">
              <Input
                id="ag-name"
                value={name}
                onChange={(e) => {
                  setName(e.target.value);
                  setError(null);
                }}
                onBlur={nameCheck.markTouched}
                aria-invalid={nameCheck.blocking || undefined}
                placeholder={t("agentPage.alice")}
                autoFocus
                className="pr-8"
              />
              <NameCheckingIcon check={nameCheck} />
            </div>
            <NameAvailabilityHint check={nameCheck} />
          </div>

          <div className="flex flex-col gap-1.5">
            <Label className="font-semibold">{t("agentPage.sessionModel")}</Label>
            <div
              className="flex flex-wrap gap-1.5"
              role="group"
              aria-label={t("agentPage.sessionModel")}
            >
              <button
                type="button"
                className={chipClass(sessionScope === "agent")}
                aria-pressed={sessionScope === "agent"}
                onClick={() => {
                  setSessionScope("agent");
                  setError(null);
                }}
              >
                {t("agentPage.acrossConversations")}
              </button>
              <button
                type="button"
                className={chipClass(sessionScope === "conversation")}
                aria-pressed={sessionScope === "conversation"}
                onClick={() => {
                  setSessionScope("conversation");
                  setError(null);
                }}
              >
                {t("agentPage.perConversation")}
              </button>
            </div>
            <p className="text-xs text-text-muted">
              {sessionScope === "agent"
                ? t("agentPage.onePersistentSessionAcross")
                : t("agentPage.independentSessionForEach")}
            </p>
          </div>

          <RuntimePicker
            hostId={selectedHostId}
            hostAvailable={selectedHostAvailable}
            value={runtime}
            onSelect={selectRuntime}
            onGateChange={setRuntimeGate}
          />

          <div className="flex flex-col gap-1.5">
            {/* No `htmlFor` → the caption is a plain visual label, not a
                click target. A `<label htmlFor="ag-model">` forwards its click
                to the Combobox input (id `ag-model`), whose `onClick` opens the
                suggestions dropdown — so clicking the "Model" text popped the
                list open (shiki 2026-07-21). The input carries its own
                `aria-label="Model"`, so a11y naming is unaffected. Matches the
                edit form's bare `<Label>Model</Label>` (RuntimeConfigEditor). */}
            <Label className="font-semibold">{t("agentPage.model")}</Label>
            <Combobox
              id="ag-model"
              ariaLabel={t("agentPage.model")}
              value={model}
              onChange={(value) => {
                setModel(value);
                setError(null);
              }}
              onCommit={setModel}
              suggestions={modelSuggestions}
              placeholder={t("agentPage.driverDefault")}
              className="h-8"
            />
          </div>

          {/* Submit-only failure line: rendered conditionally so it reserves no
              space at rest, and every field edit clears it (setError(null) in
              the handlers above) so a stale error never lingers past the change
              that fixes it. */}
          {error ? <div className="text-xs text-error">{error}</div> : null}

          <DialogFooter className="mt-2">
            <Button type="button" variant="cancel" onClick={() => onOpenChange(false)}>
              {t("common.cancel")}
            </Button>
            <Button
              type="submit"
              variant="primary"
              disabled={
                createAgent.isPending ||
                hostsQuery.isLoading ||
                (placeableHosts.length > 0 && !allSettled) ||
                runtimeGate.allAbsent ||
                nameCheck.blocking
              }
            >
              {createAgent.isPending ? t("agentPage.creating") : t("agentPage.create")}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

interface RuntimePickerProps {
  hostId: string | null;
  hostAvailable: boolean;
  value: RuntimeKind;
  onSelect: (kind: RuntimeKind) => void;
  onGateChange: (gate: RuntimeGate) => void;
}

/**
 * Self-contained runtime section for the create dialog (#task:1376). It owns
 * the per-host detection probe (`useHostRuntimeCatalog`) and its Recheck /
 * isReprobing state, so a Recheck re-renders ONLY these chips — not the host /
 * name / model fields (shiki: 只刷新 runtime 那些值的状态). The parent gets back
 * just the two gating booleans via `onGateChange`, and only when they actually
 * flip, so an unchanged Recheck never re-renders the whole form.
 *
 * Disable rules live in the pure core (`runtime-detection-ui.ts`): a chip is
 * disabled ONLY on a fresh probe of an available host reporting deterministic
 * `absent`; every probe gap (offline / probing / probe-failed) leaves the chip
 * pickable — spawn is the ground truth, so a probe hiccup must not trap an
 * installed runtime.
 */
function RuntimePicker({
  hostId,
  hostAvailable,
  value,
  onSelect,
  onGateChange,
}: RuntimePickerProps): JSX.Element {
  const runtimeCatalog = useHostRuntimeCatalog(hostId, {
    enabled: hostAvailable,
  });
  const runtimeStateByKind = useMemo(() => {
    const map = new Map<RuntimeKind, RuntimeAvailabilityState>();
    for (const info of runtimeCatalog.data?.runtimes ?? []) {
      map.set(info.kind, info.state);
    }
    return map;
  }, [runtimeCatalog.data]);

  const mode = deriveRuntimePickerMode({
    hasHost: Boolean(hostId),
    // RuntimePicker mounts only after the host availability map has resolved
    // (see the `allSettled` gate in the parent), so this axis is always
    // settled at this point — pass false explicitly to keep the pure core's
    // signature honest instead of hiding the invariant.
    isHostAvailabilityLoading: false,
    hostAvailable,
    isLoading: runtimeCatalog.isLoading,
    isError: runtimeCatalog.isError,
  });
  const detectionActive = mode === "detected";
  const sectionHint = runtimeSectionHint(mode);

  const allAbsent =
    detectionActive &&
    SELECTABLE_RUNTIMES.every((entry) => runtimeStateByKind.get(entry.kind) === "absent");
  const selectedAbsent = detectionActive && runtimeStateByKind.get(value) === "absent";
  const runtimeHint = sectionHint ?? (allAbsent ? t("agentPage.noSupportedRuntimeDetected") : null);

  // Report the gating facts up. Deps are the two primitives, so this fires only
  // when they actually flip — the isReprobing spinner toggling or a Recheck
  // that finds no change never re-renders the parent form.
  useEffect(() => {
    onGateChange({ allAbsent, selectedAbsent });
  }, [allAbsent, selectedAbsent, onGateChange]);

  // If the picked runtime is absent on this computer, move to the first
  // installed one so the form stays creatable (mirrors the host-picker's
  // offline fallback). If none are installed, leave it — Create is gated and
  // the section shows the empty state.
  useEffect(() => {
    if (!detectionActive) return;
    if (runtimeStateByKind.get(value) !== "absent") return;
    const firstInstalled = SELECTABLE_RUNTIMES.find(
      (entry) => runtimeStateByKind.get(entry.kind) !== "absent",
    );
    if (firstInstalled) onSelect(firstInstalled.kind);
  }, [detectionActive, runtimeStateByKind, value, onSelect]);

  const checking = runtimeCatalog.isLoading || runtimeCatalog.isReprobing;

  return (
    <div className="flex flex-col gap-1.5">
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-2">
          <Label required className="font-semibold">
            {t("agentPage.runtime")}
          </Label>
          {/* Light in-progress tip while a probe / Recheck runs. It lives in
              the label row so showing it never changes the section height — the
              dialog is vertically centered, so any height delta would re-center
              the whole modal and read as a jump (shiki). */}
          {checking ? (
            <span className="text-xs font-normal text-text-muted">{t("agentPage.detecting")}</span>
          ) : null}
        </div>
        {hostAvailable ? (
          <button
            type="button"
            onClick={() => void runtimeCatalog.reprobe()}
            disabled={checking}
            aria-label={t("agentPage.recheckInstalledRuntimes")}
            title={t("agentPage.recheck")}
            className="border border-transparent p-1 text-text-muted transition-colors hover:text-text-primary disabled:cursor-default disabled:opacity-60"
          >
            <RefreshIcon className={cn("size-3.5", checking && "animate-spin")} />
          </button>
        ) : null}
      </div>
      <div className="flex flex-wrap gap-2">
        {SELECTABLE_RUNTIMES.map((entry) => {
          const absent = runtimeChipDisabled(mode, runtimeStateByKind.get(entry.kind));
          const isSelected = !absent && value === entry.kind;
          const state = detectionActive ? runtimeStateByKind.get(entry.kind) : undefined;
          const title =
            state === "absent"
              ? t("agentPage.notDetectedOnThis")
              : state === "present_unverified"
                ? t("agentPage.installedVersionUnverified")
                : state === "unknown"
                  ? t("agentPage.couldntVerifyYouCan")
                  : undefined;
          return (
            <button
              key={entry.kind}
              type="button"
              disabled={absent}
              title={title}
              onClick={() => {
                if (!absent) onSelect(entry.kind);
              }}
              className={chipClass(isSelected, absent)}
              aria-pressed={isSelected}
            >
              {entry.cliLabel}
              {absent ? (
                <span className="ml-1.5 text-text-muted">{t("agentPage.notInstalled")}</span>
              ) : null}
            </button>
          );
        })}
      </div>
      {runtimeHint ? <div className="text-xs text-text-muted">{runtimeHint}</div> : null}
    </div>
  );
}

const agentCreateErrorMessages = (): Record<string, string> => ({
  local_host_ambiguous: t("agentPage.chooseWhichComputerShould"),
  local_host_unbound: t("agentPage.noConnectedComputerCan"),
  host_attach_pending: t("agentPage.thatComputerIsNot"),
  host_revoked: t("agentPage.thatComputerIsntAvailable"),
  host_not_owned: t("agentPage.thatComputerIsntAvailable"),
  host_missing: t("agentPage.thatComputerIsntAvailable"),
});

/** Map a create-agent failure to a calm, user-facing line. Known
 *  host-placement codes keep their tailored copy; new backend errors still
 *  surface their structured reason instead of disappearing behind a generic
 *  retry message. */
function friendlyCreateError(err: unknown): string {
  return describeApiError(err, {
    actionLabel: t("agentPage.couldntCreateTheAgent"),
    backendUnreachableMessage: t("agentPage.cantReachTheBackend"),
    fallbackMessage: t("agentPage.couldntCreateTheAgent2"),
    knownCodeMessages: agentCreateErrorMessages(),
  });
}
