import type { AppUpdateState } from "@silverhand/client";
import { Button } from "@/components/ui/button";
import { SpinnerIcon } from "@/icons";
import { appUpdateActions, useAppUpdateState } from "@/features/app-update/app-update-store";
import { AppUpdateProgress, describeAppUpdate } from "@/features/app-update/app-update-parts";
import { t } from "@/lib/i18n";

function statusLine(state: AppUpdateState): string | null {
  if (state.status === "idle") {
    return state.lastCheckedAt === null ? null : t("appUpdate.noNewerVersionIs");
  }
  if (state.status === "checking") {
    return null;
  }
  return describeAppUpdate(state);
}

function UpdateActions({ state }: { state: AppUpdateState }): JSX.Element | null {
  switch (state.status) {
    case "idle":
      return (
        <Button variant="neutral" size="xs" onClick={() => void appUpdateActions.check()}>
          {t("appUpdate.checkForUpdates")}
        </Button>
      );
    case "available":
      return (
        <Button variant="primary" size="xs" onClick={() => void appUpdateActions.download()}>
          {t("appUpdate.download")}
        </Button>
      );
    case "downloaded":
      return (
        <>
          <Button
            variant="neutral"
            size="xs"
            onClick={() => void appUpdateActions.install("on-quit")}
          >
            {t("appUpdate.installOnQuit")}
          </Button>
          <Button variant="primary" size="xs" onClick={() => void appUpdateActions.install("now")}>
            {t("appUpdate.restartAndInstall")}
          </Button>
        </>
      );
    case "scheduled":
      return (
        <Button variant="primary" size="xs" onClick={() => void appUpdateActions.install("now")}>
          {t("appUpdate.restartNow")}
        </Button>
      );
    case "error":
      return state.version ? (
        <Button variant="primary" size="xs" onClick={() => void appUpdateActions.download()}>
          {t("appUpdate.retryDownload")}
        </Button>
      ) : (
        <Button variant="neutral" size="xs" onClick={() => void appUpdateActions.check()}>
          {t("appUpdate.tryAgain")}
        </Button>
      );
    case "checking":
      return (
        <Button variant="neutral" size="xs" disabled aria-busy>
          <SpinnerIcon aria-hidden className="size-3 shrink-0 motion-safe:animate-spin" />
          {t("appUpdate.checking")}
        </Button>
      );
    case "downloading":
    case "installing":
      return null;
  }
}

/** The Version row. Where the app can update itself it also checks, downloads
 *  and installs in place, driven by the same state as the update dialog. */
export function AboutVersionRow({ version }: { version: string }): JSX.Element {
  const state = useAppUpdateState();
  const status = state ? statusLine(state) : null;
  const busy = state?.status === "installing";

  return (
    <div className="flex flex-col gap-2 px-3 py-2.5 text-sm">
      <div className="flex min-h-6 items-center justify-between gap-3">
        <span className="text-text-secondary">{t("settings.about.version")}</span>
        <div className="flex items-center gap-2">
          <span className="tabular-nums text-text-primary select-text">{version}</span>
          {state ? <UpdateActions state={state} /> : null}
        </div>
      </div>
      {status ? (
        <p className="flex items-center gap-2 text-xs text-text-secondary" role="status">
          {busy ? (
            <SpinnerIcon aria-hidden className="size-3.5 shrink-0 motion-safe:animate-spin" />
          ) : null}
          <span>{status}</span>
        </p>
      ) : null}
      {state?.status === "downloading" ? <AppUpdateProgress state={state} /> : null}
    </div>
  );
}
