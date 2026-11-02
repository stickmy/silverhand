import { useEffect, useSyncExternalStore } from "react";
import { onlineManager } from "@tanstack/react-query";
import { OfflineIcon, WarningIcon } from "@/icons";
import { Button } from "@/components/ui/button";
import { useBackendInfoSnapshot } from "@/features/computers/hooks/hooks";
import { useBackendUnreachableSnitch } from "./use-backend-unreachable-snitch";
import { describeProtocolSkew } from "./protocol-skew-presentation";
import { selectAppStatusBanner } from "./app-status-banner-model";
import { openAppUpdateDialog, useAppUpdateState } from "@/features/app-update/app-update-store";
import { t } from "@/lib/i18n";
import { toast } from "@/lib/toast";
import { getPlatform } from "@/platform";

const NATIVE_ID = "app-status";

function useOnline(): boolean {
  return useSyncExternalStore(
    (notify) => onlineManager.subscribe(notify),
    () => onlineManager.isOnline(),
  );
}

interface BannerShellProps {
  title: string;
  detail?: string | null;
  /** Optional trailing action. Only the app-behind skew arm offers one, and
   *  only where the app can update itself; the server-behind arm clears itself
   *  on the next poll. */
  action?: { label: string; onClick: () => void } | null;
  /** A network state, not a fault: drawn in ink rather than warning. */
  offline?: boolean;
}

function BannerShell({ title, detail, action, offline }: BannerShellProps): JSX.Element {
  return (
    <div
      role="alert"
      aria-live="polite"
      className="pointer-events-auto fixed left-1/2 top-[max(0.75rem,calc(env(safe-area-inset-top,0px)+0.5rem))] z-(--z-in-dialog) -translate-x-1/2 transform"
      // The titlebar's drag region sits right above this; the banner owns
      // its own click target so it never gets swallowed by window drag.
      style={{ WebkitAppRegion: "no-drag" } as React.CSSProperties}
    >
      <div className="flex items-center gap-3 rounded-(--floating-radius) [border:var(--floating-border)] bg-bg-floating px-3 py-2 shadow-modal">
        {offline ? (
          <OfflineIcon className="size-5 text-text-secondary" />
        ) : (
          <WarningIcon className="size-5 text-warning" />
        )}
        <div className="min-w-0">
          <p className="text-xs font-medium leading-snug text-text-primary">{title}</p>
          {detail ? (
            <p className="text-(length:--text-meta) leading-snug text-text-muted">{detail}</p>
          ) : null}
        </div>
        {action ? (
          <Button
            type="button"
            variant="ghost"
            size="xs"
            onClick={action.onClick}
            className="bg-text-primary px-2.5 text-text-inverse hover:bg-text-primary/85 hover:text-text-inverse"
          >
            {action.label}
          </Button>
        ) : null}
      </div>
    </div>
  );
}

export function AppStatusBanner(): JSX.Element | null {
  const online = useOnline();
  const unreachable = useBackendUnreachableSnitch();
  const { data } = useBackendInfoSnapshot();
  const appUpdate = useAppUpdateState();
  const skew = describeProtocolSkew(data?.outbound_protocol_compatibility ?? null);
  const model = selectAppStatusBanner({
    online,
    unreachableVisible: unreachable.visible,
    skew,
  });
  const appBehind = data?.outbound_protocol_compatibility?.status === "app_update_required";

  const shell: BannerShellProps | null = !model
    ? null
    : model.kind === "offline"
      ? { title: t("appShell.offline"), detail: t("appShell.offlineDetail"), offline: true }
      : model.kind === "backend_unreachable"
        ? {
            title: t("appShell.serverUnreachable"),
            action: { label: t("common.retry"), onClick: unreachable.retry },
          }
        : {
            title: model.view.title,
            detail: model.view.detail,
            action:
              appBehind && appUpdate
                ? { label: t("common.update"), onClick: openAppUpdateDialog }
                : null,
          };
  // Native bars cover a web banner, so a host that draws toasts shows it as a
  // sticky one instead.
  const native = Boolean(getPlatform().toast);
  const title = shell?.title;
  const detail = shell?.detail ?? undefined;
  const action = shell?.action ?? undefined;
  const offline = shell?.offline ?? false;

  useEffect(() => {
    if (!native) {
      return;
    }
    if (!title) {
      toast.dismiss(NATIVE_ID);
      return;
    }
    const options = { id: NATIVE_ID, description: detail, duration: Infinity, action };
    if (offline) {
      toast(title, { ...options, glyph: "offline" });
    } else {
      toast.warning(title, options);
    }
  }, [native, title, detail, action?.label, offline]);

  if (!shell || native) return null;
  return <BannerShell {...shell} />;
}
