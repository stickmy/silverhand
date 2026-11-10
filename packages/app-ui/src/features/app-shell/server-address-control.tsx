import { useState } from "react";
import { useQueryClient, type QueryClient } from "@tanstack/react-query";
import { ChevronRightIcon, ErrorIcon, RetryIcon, SpinnerIcon } from "@/icons";
import { cn } from "@/lib/cn";
import { getPlatform } from "@/platform";
import { Button } from "@/components/ui/button";
import { Input, Label } from "@/components/ui/input";
import { validateServerUrlDraft } from "@/features/settings/model/server-url-validation";
import {
  refreshServerEnvironment,
  useServerEnvironment,
} from "@/features/session/server-environment";
import { describeProbeOutcome } from "@/features/settings/model/describe-probe-outcome";
import type { SaveServerUrlBanner } from "@/features/settings/model/describe-save-server-url-outcome";
import type { ProfileSaveWithTestResult } from "@silverhand/client";
import { t } from "@/lib/i18n";

/** Checks and saves a new server origin; the message is why it was not saved. */
export async function saveServerAddress(qc: QueryClient, draft: string): Promise<string | null> {
  try {
    const policy = await getPlatform().client.getConnectionPolicy();
    if (
      validateServerUrlDraft(draft, { allowInsecureRemote: policy.allow_insecure_remote }).kind !==
      "ok"
    ) {
      return t("appShell.server.invalid");
    }
    const result = await getPlatform().client.saveServerConnectionProfile({
      serverUrl: draft.trim(),
    });
    if (!result.ok) {
      const message = describeProfileSaveForGate(result);
      return message.description ?? message.title;
    }
    await refreshServerEnvironment(qc);
    return null;
  } catch (err) {
    return String(err);
  }
}

/** Remote-only auth gates offer an escape to a different self-hosted origin. */
export function ServerAddressControl({
  onRetry,
  disabled = false,
  onEditingChange,
}: {
  disabled?: boolean;
  onEditingChange?: (editing: boolean) => void;
  onRetry?: () => void;
}) {
  const qc = useQueryClient();
  const environment = useServerEnvironment();
  const [draft, setDraft] = useState("");
  const [editing, setEditing] = useState(false);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);

  const url = environment.data?.url ?? "";
  const error = saveError ?? (environment.isError ? t("appShell.server.readFailed") : null);

  const save = async () => {
    if (saving || disabled) return;
    setSaving(true);
    setSaveError(null);
    const failure = await saveServerAddress(qc, draft);
    setSaving(false);
    if (failure) {
      setSaveError(failure);
      return;
    }
    setEditing(false);
    onEditingChange?.(false);
    onRetry?.();
  };
  return (
    <section
      className={cn("min-w-0 space-y-2", editing && "basis-full")}
      aria-label={t("appShell.server.address")}
    >
      {editing ? (
        <>
          <Label htmlFor="login-server">{t("appShell.server.address")}</Label>
          <Input
            id="login-server"
            type="url"
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            disabled={saving || disabled}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                event.stopPropagation();
                void save();
              }
            }}
          />
          <div className="grid grid-cols-2 gap-3 md:flex md:justify-end md:gap-2">
            <Button
              type="button"
              variant="primary"
              size="sm"
              disabled={saving || disabled}
              onClick={() => {
                void save();
              }}
            >
              {saving ? t("common.checking") : t("common.save")}
            </Button>
            <Button
              type="button"
              variant="cancel"
              size="sm"
              disabled={saving || disabled}
              onClick={() => {
                setEditing(false);
                setSaveError(null);
                onEditingChange?.(false);
              }}
            >
              {t("common.cancel")}
            </Button>
          </div>
        </>
      ) : (
        <button
          type="button"
          aria-label={t("appShell.server.change")}
          title={url}
          className="flex min-w-0 max-w-full items-center gap-0.5 text-text-muted hover:text-text-secondary focus-outline disabled:opacity-50 max-md:min-h-11"
          disabled={saving || disabled}
          onClick={() => {
            setDraft(url);
            setEditing(true);
            onEditingChange?.(true);
          }}
        >
          <span className="truncate">
            {url ? url.replace(/^https?:\/\//, "").replace(/\/$/, "") : t("appShell.server.notSet")}
          </span>
          <ChevronRightIcon className="size-3.5 shrink-0" aria-hidden />
        </button>
      )}
      {error && (
        <p role="alert" className="text-error">
          {error}
        </p>
      )}
    </section>
  );
}

/** Connection state for the auth gate. Reads as part of the server row it
 *  describes, below the form, rather than as a banner over it. */
export function ServerConnectionStatus({
  status,
  checking = false,
  onRetry,
}: {
  status?: string;
  checking?: boolean;
  onRetry?: () => void;
}) {
  if (checking) {
    return (
      <div role="status" className="mt-3 flex items-center gap-2 text-xs text-text-muted">
        <SpinnerIcon className="size-3.5 animate-spin" aria-hidden="true" />
        {t("appShell.server.checking")}
      </div>
    );
  }
  if (!status) {
    return null;
  }

  const failure = describeConnectionFailure(status);

  return (
    <section
      className="mt-3 flex flex-col gap-2 text-xs"
      aria-label={t("appShell.server.connection")}
    >
      <div role="alert" className="flex items-start gap-2">
        <ErrorIcon className="mt-px size-3.5 shrink-0 text-error" aria-hidden="true" />
        <p className="min-w-0 leading-relaxed text-text-secondary">
          <span className="font-medium text-text-primary">{failure.title}</span> {failure.detail}
        </p>
      </div>
      <div className="flex items-start justify-between gap-3 pl-5.5">
        <details className="min-w-0 text-text-muted">
          <summary className="cursor-pointer focus-outline">{t("common.technicalDetails")}</summary>
          <p className="mt-1 break-words leading-relaxed">{failure.technical}</p>
        </details>
        {onRetry && (
          <Button type="button" variant="neutral" size="sm" className="shrink-0" onClick={onRetry}>
            <RetryIcon className="size-3.5" aria-hidden="true" />
            {t("common.retry")}
          </Button>
        )}
      </div>
    </section>
  );
}

/** Why the auth gate cannot reach its server, in the gate's words. */
export function describeConnectionFailure(status: string) {
  const secure = /TLS|SSL|certificate|secure connection/i.test(status);

  return {
    title: secure ? t("appShell.server.secureFailed") : t("appShell.server.cannotConnect"),
    detail: secure ? t("appShell.server.signInUnavailable") : t("appShell.server.checkConnection"),
    technical: status.replace(/^Error:\s*/, ""),
  };
}

export function describeProfileSaveForGate(result: ProfileSaveWithTestResult): SaveServerUrlBanner {
  if (result.ok) {
    return {
      tone: "success",
      title: t("appShell.unreachable.savedRetrying"),
      description: t("appShell.unreachable.using", { url: result.profile.server_url }),
    };
  }
  if (result.phase === "test") {
    const banner = describeProbeOutcome(result.test);
    return {
      tone: banner.tone === "success" ? "success" : "error",
      title: banner.title,
      description: banner.description,
    };
  }
  if (result.phase === "commit_guard") {
    return {
      tone: "error",
      title: t("appShell.unreachable.commitGuard"),
      description: result.message,
    };
  }
  return {
    tone: "error",
    title: t("appShell.unreachable.writeFailed"),
    description: result.detail ?? t("appShell.unreachable.writeFailedDetail"),
  };
}
