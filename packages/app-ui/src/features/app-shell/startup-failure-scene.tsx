import { useEffect, useState, type FormEvent, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import type { BootCapability, BootState } from "@silverhand/client";
import { ChevronRightIcon, CopyIcon, FolderIcon, OpenExternalIcon, SpinnerIcon } from "@/icons";
import { Button } from "@/components/ui/button";
import { Input, Label } from "@/components/ui/input";
import { useAppearance } from "@/features/appearance/use-appearance";
import { cn } from "@/lib/cn";
import { t } from "@/lib/i18n";
import { getPlatform } from "@/platform";
import { AuthSplitLayout } from "./auth-split-layout";
import { checkAndSaveServerAddress } from "./server-address-control";

type FailedBoot = Extract<BootState, { kind: "failed" }>;

const COPIED_MS = 2000;

/**
 * Holds the app until the host says startup succeeded. A failed startup has no
 * backend behind it, so nothing below this gate may mount: every session and
 * backend query would fail against a host that never came up.
 */
export function BootGate({ children }: { children: ReactNode }): JSX.Element {
  const boot = getPlatform().boot;
  const [state, setState] = useState<BootState | null>(boot ? null : { kind: "ready" });

  useEffect(() => {
    if (!boot) {
      return;
    }
    let live = true;
    void boot.getState().then((next) => {
      if (live) {
        setState(next);
      }
    });
    return () => {
      live = false;
    };
  }, [boot]);

  if (!state) {
    return <div className="h-screen w-screen bg-bg-base" />;
  }
  if (state.kind === "failed" && boot) {
    return <StartupFailureScene state={state} boot={boot} />;
  }
  return <>{children}</>;
}

function StartupFailureScene({
  state,
  boot,
}: {
  state: FailedBoot;
  boot: BootCapability;
}): JSX.Element {
  const repairAddress = state.repair === "server_address";

  // The app below the gate is what normally applies the stored theme.
  useAppearance();

  return (
    <AuthSplitLayout quiet>
      <div
        data-startup-failure={state.notice.code}
        className="flex w-full max-w-90 flex-col"
        style={{ WebkitAppRegion: "no-drag" } as React.CSSProperties}
      >
        <header className="mb-6 flex flex-col gap-1.5">
          <h1 className="text-3xl font-semibold tracking-tight">{t("appShell.startup.title")}</h1>
          <p className="text-sm text-text-muted">{failureReason(state)}</p>
        </header>

        {repairAddress ? (
          <AddressRepair state={state} boot={boot} />
        ) : (
          <div className="flex flex-col gap-4">
            {state.notice.detail ? (
              <p className="wrap-break-word rounded-(--control-radius) bg-(--chip-bg) px-3 py-2.5 font-mono text-xs leading-relaxed text-text-secondary">
                {state.notice.detail}
              </p>
            ) : null}
            <div className="flex gap-2">
              <Button
                type="button"
                variant="primary"
                size="md"
                className="flex-1"
                onClick={() => void boot.relaunch()}
              >
                {t("appShell.startup.relaunch")}
              </Button>
              <QuitButton boot={boot} />
            </div>
          </div>
        )}

        <Diagnostics state={state} boot={boot} defaultOpen={!repairAddress} />
      </div>
    </AuthSplitLayout>
  );
}

function AddressRepair({ state, boot }: { state: FailedBoot; boot: BootCapability }) {
  const qc = useQueryClient();
  const [draft, setDraft] = useState(state.notice.profileUrl ?? "");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<{ title: string; detail: string | null } | null>(null);

  const submit = async (event: FormEvent<HTMLFormElement>): Promise<void> => {
    event.preventDefault();
    if (saving) {
      return;
    }
    setSaving(true);
    setError(null);
    const failure = await checkAndSaveServerAddress(qc, draft);
    if (failure) {
      setSaving(false);
      setError(failure);
      return;
    }
    await boot.relaunch();
  };

  return (
    <form onSubmit={(event) => void submit(event)} className="flex flex-col gap-4">
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="startup-server">{t("appShell.server.address")}</Label>
        <Input
          id="startup-server"
          type="url"
          autoFocus
          value={draft}
          placeholder="https://silverhand.example.com"
          aria-invalid={error ? true : undefined}
          disabled={saving}
          className={cn(error && "border-error focus-visible:border-error")}
          onChange={(event) => setDraft(event.target.value)}
        />
        {error ? (
          <div role="alert" className="flex flex-col gap-0.5 text-xs">
            <p className="text-error">{error.title}</p>
            {error.detail ? (
              <p className="wrap-break-word text-text-muted">{error.detail}</p>
            ) : null}
          </div>
        ) : null}
      </div>
      <div className="flex gap-2">
        <Button
          type="submit"
          variant="primary"
          size="md"
          className="flex-1"
          disabled={saving || draft.trim() === ""}
        >
          {saving ? <SpinnerIcon className="size-4 animate-spin" aria-hidden /> : null}
          {saving ? t("appShell.startup.connecting") : t("appShell.startup.connect")}
        </Button>
        <QuitButton boot={boot} disabled={saving} />
      </div>
    </form>
  );
}

function QuitButton({ boot, disabled = false }: { boot: BootCapability; disabled?: boolean }) {
  return (
    <Button
      type="button"
      variant="cancel"
      size="md"
      disabled={disabled}
      onClick={() => void boot.quit()}
    >
      {t("appShell.startup.quit")}
    </Button>
  );
}

function Diagnostics({
  state,
  boot,
  defaultOpen,
}: {
  state: FailedBoot;
  boot: BootCapability;
  defaultOpen: boolean;
}) {
  const [open, setOpen] = useState(defaultOpen);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    if (!copied) {
      return;
    }
    const timer = window.setTimeout(() => setCopied(false), COPIED_MS);
    return () => window.clearTimeout(timer);
  }, [copied]);

  const { notice, diagnostics } = state;
  const commit = diagnostics.commit.slice(0, 9);
  const build = diagnostics.dirty ? `${commit} ${t("appShell.startup.modified")}` : commit;
  const version = diagnostics.version ? `${diagnostics.version} (${build})` : build;
  const addressDetail = state.repair === "server_address" ? notice.detail : null;

  return (
    <section className="mt-8 border-t border-border-subtle pt-4 text-xs">
      <button
        type="button"
        aria-expanded={open}
        className="flex items-center gap-1 text-text-muted hover:text-text-secondary focus-outline"
        onClick={() => setOpen((value) => !value)}
      >
        <ChevronRightIcon
          className={cn("size-3.5 shrink-0 transition-transform", open && "rotate-90")}
          aria-hidden
        />
        {t("appShell.startup.diagnostics")}
      </button>

      {open ? (
        <div className="mt-3 pl-4.5">
          <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1.5">
            <dt className="text-text-muted">{t("appShell.startup.code")}</dt>
            <dd className="break-all font-mono text-text-secondary">{notice.code}</dd>
            {addressDetail ? (
              <>
                <dt className="text-text-muted">{t("appShell.startup.detail")}</dt>
                <dd className="wrap-break-word text-text-secondary">{addressDetail}</dd>
              </>
            ) : null}
            <dt className="text-text-muted">{t("appShell.startup.version")}</dt>
            <dd className="break-all font-mono text-text-secondary">{version}</dd>
            {diagnostics.logsDir ? (
              <>
                <dt className="text-text-muted">{t("appShell.startup.logs")}</dt>
                <dd className="break-all font-mono text-text-secondary">{diagnostics.logsDir}</dd>
              </>
            ) : null}
          </dl>
          <div className="mt-3 flex flex-wrap gap-x-4 gap-y-2">
            <DiagnosticAction
              icon={<CopyIcon className="size-3.5" aria-hidden />}
              onClick={() => {
                void boot.copyDiagnostics().then(() => setCopied(true));
              }}
            >
              {copied ? t("appShell.startup.copied") : t("appShell.startup.copy")}
            </DiagnosticAction>
            {diagnostics.logsDir ? (
              <DiagnosticAction
                icon={<FolderIcon className="size-3.5" aria-hidden />}
                onClick={() => void boot.revealLogs()}
              >
                {t("appShell.startup.openLogs")}
              </DiagnosticAction>
            ) : null}
            <DiagnosticAction
              icon={<OpenExternalIcon className="size-3.5" aria-hidden />}
              onClick={() => void boot.reportIssue()}
            >
              {t("appShell.startup.report")}
            </DiagnosticAction>
          </div>
        </div>
      ) : null}
    </section>
  );
}

function DiagnosticAction({
  icon,
  onClick,
  children,
}: {
  icon: ReactNode;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      className="flex items-center gap-1 text-text-muted hover:text-text-secondary focus-outline"
      onClick={onClick}
    >
      {icon}
      {children}
    </button>
  );
}

function failureReason(state: FailedBoot): string {
  switch (state.notice.code) {
    case "remote_profile_required": {
      return t("appShell.startup.needsServer");
    }
    case "profile_invalid": {
      return t("appShell.startup.savedUnusable");
    }
    default: {
      return t("appShell.startup.failed");
    }
  }
}
