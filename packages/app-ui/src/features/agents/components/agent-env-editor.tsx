import { useState, type JSX } from "react";
import {
  AGENT_ENV_VAR_NAME_PATTERN,
  MAX_AGENT_ENV_VAR_NAME_LENGTH,
  MAX_AGENT_ENV_VAR_VALUE_LENGTH,
  MAX_AGENT_ENV_VARS,
  type AgentEnvVar,
} from "@silverhand/shared";
import { CloseIcon, PlusIcon, ViewIcon, ViewOffIcon } from "@/icons";
import { Button } from "@/components/ui/button";
import { Input, Label } from "@/components/ui/input";
import { t } from "@/lib/i18n";

export interface EnvDraftRow {
  id: string;
  name: string;
  value: string;
}

export type EnvRowErrorKind = "required" | "invalid_name" | "duplicate" | "invalid_value";

export interface EnvDraftValidation {
  ok: boolean;
  tooMany: boolean;
  rowErrors: ReadonlyMap<string, EnvRowErrorKind>;
}

let nextDraftRowId = 1;

export function createEmptyEnvRow(): EnvDraftRow {
  const id = `env-row-${nextDraftRowId}`;
  nextDraftRowId += 1;
  return { id, name: "", value: "" };
}

export function envDraftFromVars(vars: readonly AgentEnvVar[]): EnvDraftRow[] {
  return vars.map((entry, index) => ({
    id: `saved-${index}-${entry.name}`,
    name: entry.name,
    value: entry.value,
  }));
}

/** Drop completely blank rows (empty name and empty value) before comparing or saving. */
export function normalizeEnvDraft(rows: readonly EnvDraftRow[]): AgentEnvVar[] {
  const normalized: AgentEnvVar[] = [];
  for (const row of rows) {
    const trimmedName = row.name.trim();
    if (trimmedName.length === 0 && row.value.length === 0) {
      continue;
    }
    normalized.push({ name: trimmedName, value: row.value });
  }
  return normalized;
}

export function envDraftIsDirty(
  saved: readonly AgentEnvVar[],
  rows: readonly EnvDraftRow[],
): boolean {
  const next = normalizeEnvDraft(rows);
  if (next.length !== saved.length) {
    return true;
  }
  for (let i = 0; i < saved.length; i += 1) {
    if (saved[i]!.name !== next[i]!.name || saved[i]!.value !== next[i]!.value) {
      return true;
    }
  }
  return false;
}

export function validateEnvDraft(rows: readonly EnvDraftRow[]): EnvDraftValidation {
  const rowErrors = new Map<string, EnvRowErrorKind>();
  const seen = new Set<string>();
  let activeCount = 0;

  for (const row of rows) {
    const trimmedName = row.name.trim();
    if (trimmedName.length === 0 && row.value.length === 0) {
      continue;
    }
    activeCount += 1;

    if (trimmedName.length === 0) {
      rowErrors.set(row.id, "required");
      continue;
    }

    if (
      trimmedName.length > MAX_AGENT_ENV_VAR_NAME_LENGTH ||
      !AGENT_ENV_VAR_NAME_PATTERN.test(trimmedName)
    ) {
      rowErrors.set(row.id, "invalid_name");
      continue;
    }

    if (seen.has(trimmedName)) {
      rowErrors.set(row.id, "duplicate");
      continue;
    }
    seen.add(trimmedName);

    if (row.value.length > MAX_AGENT_ENV_VAR_VALUE_LENGTH || row.value.includes("\0")) {
      rowErrors.set(row.id, "invalid_value");
    }
  }

  const tooMany = activeCount > MAX_AGENT_ENV_VARS;
  return {
    ok: !tooMany && rowErrors.size === 0,
    tooMany,
    rowErrors,
  };
}

/** Split a pasted `NAME=VALUE` line when pasted into the Name field. */
export function parseEnvPasteLine(raw: string): { name: string; value: string } | null {
  const line = raw.trim();
  if (!line || line.includes("\n") || line.includes("\r")) {
    return null;
  }
  const eqIndex = line.indexOf("=");
  if (eqIndex <= 0) {
    return null;
  }
  const name = line.slice(0, eqIndex).trim();
  if (!AGENT_ENV_VAR_NAME_PATTERN.test(name)) {
    return null;
  }
  return { name, value: line.slice(eqIndex + 1) };
}

function rowErrorMessage(kind: EnvRowErrorKind): string {
  switch (kind) {
    case "required":
      return t("agents.envVarNameRequired");
    case "invalid_name":
      return t("agents.envVarInvalidName");
    case "duplicate":
      return t("agents.envVarDuplicateName");
    case "invalid_value":
      return t("agents.envVarValueInvalid");
  }
}

export function AgentEnvReadOnly({
  vars,
  loading,
  error,
  onRetry,
}: {
  vars: readonly AgentEnvVar[];
  loading: boolean;
  error: boolean;
  onRetry: () => void;
}): JSX.Element {
  const [revealed, setRevealed] = useState<ReadonlySet<string>>(() => new Set());

  function toggleReveal(name: string) {
    setRevealed((prev) => {
      const next = new Set(prev);
      if (next.has(name)) {
        next.delete(name);
      } else {
        next.add(name);
      }
      return next;
    });
  }

  return (
    <div className="flex min-w-0 flex-col gap-1.5">
      <span className="text-xs text-text-muted">{t("agents.environmentVariables")}</span>

      {loading ? (
        <span className="text-sm text-text-muted italic">{t("agents.loading")}</span>
      ) : error ? (
        <div className="flex items-center gap-2 text-xs text-error">
          <span>{t("agents.envVarLoadFailed")}</span>
          <button
            type="button"
            onClick={onRetry}
            className="text-text-secondary underline underline-offset-2 hover:text-text-primary"
          >
            {t("agents.envVarRetry")}
          </button>
        </div>
      ) : vars.length === 0 ? (
        <span className="text-sm text-text-muted italic">{t("agents.noEnvironmentVariables")}</span>
      ) : (
        <div className="flex flex-col gap-1">
          {vars.map((entry) => {
            const isRevealed = revealed.has(entry.name);
            const hasValue = entry.value.length > 0;
            return (
              <div
                key={entry.name}
                className="grid grid-cols-[minmax(0,1fr)_minmax(0,1.4fr)_auto] items-center gap-2 text-xs"
              >
                <span className="truncate font-mono text-text-primary" title={entry.name}>
                  {entry.name}
                </span>
                <span
                  className={
                    hasValue ? "truncate font-mono text-text-secondary" : "text-text-muted italic"
                  }
                  title={isRevealed && hasValue ? entry.value : undefined}
                >
                  {!hasValue ? t("agents.envVarEmptyValue") : isRevealed ? entry.value : "***"}
                </span>
                {hasValue ? (
                  <button
                    type="button"
                    onClick={() => toggleReveal(entry.name)}
                    aria-label={
                      isRevealed ? t("agents.envVarHideValue") : t("agents.envVarShowValue")
                    }
                    title={isRevealed ? t("agents.envVarHideValue") : t("agents.envVarShowValue")}
                    className="inline-flex size-6 items-center justify-center rounded-sm text-text-muted transition-colors hover:text-text-primary"
                  >
                    {isRevealed ? (
                      <ViewOffIcon className="size-3.5" />
                    ) : (
                      <ViewIcon className="size-3.5" />
                    )}
                  </button>
                ) : (
                  <span className="size-6" aria-hidden />
                )}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

export function AgentEnvEditList({
  rows,
  onChange,
  validation,
  loading,
  error,
  onRetry,
  disabled,
}: {
  rows: readonly EnvDraftRow[];
  onChange: (next: EnvDraftRow[]) => void;
  validation: EnvDraftValidation;
  loading: boolean;
  error: boolean;
  onRetry: () => void;
  disabled: boolean;
}): JSX.Element {
  function updateRow(id: string, patch: Partial<Pick<EnvDraftRow, "name" | "value">>) {
    onChange(rows.map((row) => (row.id === id ? { ...row, ...patch } : row)));
  }

  function removeRow(id: string) {
    onChange(rows.filter((row) => row.id !== id));
  }

  function addRow() {
    onChange([...rows, createEmptyEnvRow()]);
  }

  const atLimit = rows.length >= MAX_AGENT_ENV_VARS;

  return (
    <div className="flex min-w-0 flex-col gap-2">
      <Label>{t("agents.environmentVariables")}</Label>

      {loading ? (
        <span className="text-sm text-text-muted italic">{t("agents.loading")}</span>
      ) : error ? (
        <div className="flex items-center gap-2 text-xs text-error">
          <span>{t("agents.envVarLoadFailed")}</span>
          <button
            type="button"
            onClick={onRetry}
            className="text-text-secondary underline underline-offset-2 hover:text-text-primary"
          >
            {t("agents.envVarRetry")}
          </button>
        </div>
      ) : (
        <>
          {rows.length > 0 ? (
            <div className="flex flex-col gap-2">
              {rows.map((row) => {
                const rowError = validation.rowErrors.get(row.id);
                return (
                  <div key={row.id} className="flex flex-col gap-1">
                    <div className="grid grid-cols-[minmax(0,1fr)_minmax(0,1.4fr)_auto] items-center gap-2">
                      <Input
                        value={row.name}
                        disabled={disabled}
                        placeholder={t("agents.envVarNamePlaceholder")}
                        aria-label={t("agents.envVarNamePlaceholder")}
                        autoComplete="off"
                        spellCheck={false}
                        className="h-8 bg-bg-base px-2.5 font-mono text-xs"
                        onChange={(e) => updateRow(row.id, { name: e.target.value })}
                        onPaste={(e) => {
                          if (row.value.length > 0) return;
                          const parsed = parseEnvPasteLine(e.clipboardData.getData("text"));
                          if (!parsed) return;
                          e.preventDefault();
                          updateRow(row.id, parsed);
                        }}
                      />
                      <Input
                        value={row.value}
                        disabled={disabled}
                        placeholder={t("agents.envVarValuePlaceholder")}
                        aria-label={t("agents.envVarValuePlaceholder")}
                        autoComplete="off"
                        spellCheck={false}
                        className="h-8 bg-bg-base px-2.5 font-mono text-xs"
                        onChange={(e) => updateRow(row.id, { value: e.target.value })}
                      />
                      <button
                        type="button"
                        disabled={disabled}
                        onClick={() => removeRow(row.id)}
                        aria-label={t("agents.removeEnvironmentVariable")}
                        title={t("agents.removeEnvironmentVariable")}
                        className="inline-flex size-8 items-center justify-center rounded-(--control-radius) text-text-muted transition-colors hover:bg-bg-hover hover:text-text-primary disabled:cursor-not-allowed disabled:opacity-50"
                      >
                        <CloseIcon className="size-3.5" />
                      </button>
                    </div>
                    {rowError ? (
                      <div className="text-xs text-error">{rowErrorMessage(rowError)}</div>
                    ) : null}
                  </div>
                );
              })}
            </div>
          ) : null}

          {validation.tooMany ? (
            <div className="text-xs text-error">{t("agents.envVarTooMany")}</div>
          ) : null}

          <div>
            <Button
              type="button"
              variant="ghost"
              size="sm"
              disabled={disabled || atLimit}
              onClick={addRow}
              className="h-7 gap-1 px-2 text-xs text-text-secondary hover:text-text-primary"
            >
              <PlusIcon className="size-3.5" />
              {t("agents.addEnvironmentVariable")}
            </Button>
          </div>
        </>
      )}
    </div>
  );
}
