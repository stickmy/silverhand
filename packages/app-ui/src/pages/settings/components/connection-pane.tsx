import { getPlatform, requireLocalHost } from "@/platform";
import { useEffect, useMemo, useRef, useState, type FormEvent } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { SaveIcon, SpinnerIcon } from "@/icons";
import type { ProfileSaveWithTestResult } from "@silverhand/client";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { cn } from "@/lib/cn";
import { SettingsGroup, SettingsPage, SettingsRow, SettingsSection } from "./settings-section";
import {
  backendInfoKey,
  hostsKey,
  hostStatusKey,
  useBackendInfoSnapshot,
} from "@/features/computers/hooks/hooks";
import { refreshServerEnvironment } from "@/features/session/server-environment";
import { computerCopy } from "@/features/computers/model/computer-copy";
import { eventWsClient, type WsClientStatus } from "@/lib/ws-client";
import { describeProbeOutcome } from "@/features/settings/model/describe-probe-outcome";
import {
  describeSaveServerUrlOutcome,
  type SaveServerUrlBanner,
} from "../../../features/settings/model/describe-save-server-url-outcome";
import {
  planServerUrlSave,
  runServerUrlSave,
  type ServerUrlSaveCommand,
  type ServerUrlSaveResult,
} from "../model/server-url-save-command";
import { validateServerUrlDraft } from "../../../features/settings/model/server-url-validation";
import {
  desktopServerConnectionStatus,
  isDesktopServerReadyAfterSave,
  type DesktopServerConnectionStatus,
  type DesktopServerObservation,
} from "../model/connection-readiness";
import { t } from "@/lib/i18n";

/**
 * Availability words come from the `computerCopy` single source — the
 * Unified Computer model forbids one surface saying "Connected" while
 * another says "Ready"/"Online" for the same availability fact (§7.1, no
 * source-fork), and a local literal here is exactly how that fork starts.
 * `Connecting…` stays literal: it is the Desktop→Server attach window,
 * a state the availability lexicon has no word for.
 */
const statusLabel = (): Record<Exclude<DesktopServerConnectionStatus, "unknown">, string> => ({
  connected: computerCopy().availabilityConnected,
  connecting: t("settings.connection.connecting"),
  offline: computerCopy().availabilityOffline,
});

const STATUS_DOT_CLASS: Record<Exclude<DesktopServerConnectionStatus, "unknown">, string> = {
  connected: "bg-success",
  connecting: "bg-text-muted",
  offline: "bg-warning",
};

function ServerStatusRow({
  status,
}: {
  status: DesktopServerConnectionStatus;
}): JSX.Element | null {
  // `unknown` = no BackendInfo observed yet. Render no status row rather than a
  // status word we have not earned: every candidate ("Connecting…",
  // "Offline") is a claim about a connection nobody has looked at, and a
  // word that later walks backwards reads as a glitch (`#task:1630`
  // review).
  if (status === "unknown") {
    return null;
  }

  return (
    <SettingsRow
      label={t("settings.connection.status")}
      control={
        <span
          role="status"
          aria-label={t("settings.connection.statusLabel", { status: statusLabel()[status] })}
          className="inline-flex shrink-0 items-center gap-2 text-sm text-text-primary"
        >
          <span
            aria-hidden
            className={cn("size-2 shrink-0 rounded-full", STATUS_DOT_CLASS[status])}
          />
          {statusLabel()[status]}
        </span>
      }
    />
  );
}

function describeProfileSaveForConnection(result: ProfileSaveWithTestResult): SaveServerUrlBanner {
  if (result.ok) {
    return {
      tone: "info",
      title: t("settings.connection.savedReconnecting"),
      description: t("appShell.unreachable.using", { url: result.profile.server_url }),
      dismissOnReady: true,
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

export function ConnectionPane(): JSX.Element {
  const qc = useQueryClient();

  const backendInfoQuery = useBackendInfoSnapshot({
    pollWhileConnecting: true,
  });

  const snapshot = backendInfoQuery.data ?? null;

  const observation: DesktopServerObservation | null =
    snapshot?.local_host_ws_attachment && snapshot.local_host_status
      ? {
          attachment: snapshot.local_host_ws_attachment,
          localHostStatus: snapshot.local_host_status,
        }
      : null;

  // Renderer event WS status — authoritative Desktop→Server signal in
  // remote mode (in embedded the WS is against embedded main, so the
  // pip falls back to the daemon attachment via the derivation).
  const [wsStatus, setWsStatus] = useState<WsClientStatus>(() => eventWsClient.getStatus());
  const wsConnectedAtRef = useRef<number | null>(
    eventWsClient.getStatus() === "connected" ? Date.now() : null,
  );
  useEffect(() => {
    return eventWsClient.onStatus((next) => {
      setWsStatus(next);
      if (next === "connected") {
        wsConnectedAtRef.current = Date.now();
      }
    });
  }, []);

  const connectionStatus = getPlatform().localHost
    ? desktopServerConnectionStatus({ observation, wsStatus })
    : wsStatus === "connected"
      ? "connected"
      : wsStatus === "connecting" || wsStatus === "authenticating"
        ? "connecting"
        : "offline";

  // ---- Server address form -------------------------------------------------
  // Source-of-truth pin:
  // - Embedded mode edits the daemon outbound URL, so seed from
  //   `local_host_server_url`.
  // - Remote mode edits the app's active server profile, so seed from
  //   `url`. A previous legacy Settings save may have already changed
  //   daemon-config without changing the profile; showing daemon-config there
  //   would make the form look unchanged while the app is still on the old
  //   backend.
  // - No snapshot: no source of truth to show yet, so the field seeds
  //   empty (and Save stays disabled via `saveCommand === null` below).
  const currentUrl =
    snapshot === null
      ? ""
      : snapshot.mode === "remote"
        ? snapshot.url
        : (snapshot.local_host_server_url ?? snapshot.url);

  const [draft, setDraft] = useState<string>("");
  const draftEditedRef = useRef(false);
  useEffect(() => {
    if (draftEditedRef.current) return;
    setDraft(currentUrl);
  }, [currentUrl]);

  const [banner, setBanner] = useState<SaveServerUrlBanner | null>(null);
  const [savedAt, setSavedAt] = useState<number | null>(null);
  const [allowInsecureRemote, setAllowInsecureRemote] = useState(false);
  useEffect(() => {
    let cancelled = false;
    getPlatform()
      .client.getConnectionPolicy()
      .then((config) => {
        if (!cancelled) setAllowInsecureRemote(config.allow_insecure_remote);
      })
      .catch(() => {
        if (!cancelled) setAllowInsecureRemote(false);
      });
    return () => {
      cancelled = true;
    };
  }, []);
  const updateDraft = (value: string): void => {
    setDraft(value);
    draftEditedRef.current = true;
    setBanner(null);
    setSavedAt(null);
  };

  const save = useMutation({
    // The variables ARE the routing decision (`planServerUrlSave`), so
    // this function cannot re-guess which owner should receive the
    // address — there is no `mode` left for it to default.
    mutationFn: async (command: ServerUrlSaveCommand): Promise<ServerUrlSaveResult> =>
      runServerUrlSave(command, {
        saveServerConnectionProfile: (input) =>
          getPlatform().client.saveServerConnectionProfile(input),
        saveServerUrl: (input) => requireLocalHost().saveServerUrl(input),
      }),
    onSuccess: (saveResult) => {
      const nextBanner =
        saveResult.source === "profile"
          ? describeProfileSaveForConnection(saveResult.result)
          : describeSaveServerUrlOutcome(saveResult.result);
      setBanner(nextBanner);
      setSavedAt(nextBanner.dismissOnReady ? Date.now() : null);
      if (saveResult.result.ok) {
        draftEditedRef.current = false;

        if (saveResult.source === "profile") {
          // Remote-mode Settings edits must retarget the renderer transport too.
          // The profile pipeline updates main's active backend handle; forcing a
          // reconnect makes event WS re-read BackendInfo and dial the new origin
          // instead of leaving the old server socket open indefinitely.
          eventWsClient.disconnect();
          eventWsClient.connect();
        }
        void qc.invalidateQueries({ queryKey: backendInfoKey });
        void refreshServerEnvironment(qc);
        void qc.invalidateQueries({ queryKey: hostsKey });
        const hostId = snapshot?.local_host_id ?? null;
        if (hostId !== null) {
          void qc.invalidateQueries({ queryKey: hostStatusKey(hostId) });
        }
      }
    },
    onError: (err) => {
      setSavedAt(null);
      setBanner({
        tone: "error",
        title: t("settings.connection.saveFailed"),
        description: err instanceof Error ? err.message : t("common.unknownError"),
      });
    },
  });

  // Dismiss the "Saved. Reconnecting…" banner only on an authoritative
  // Desktop→Server reachability proof observed AFTER save started —
  // renderer WS `connected` (attachment unobservable / remote) or daemon
  // `attached_at` moving forward past `savedAt` (observable / embedded).
  // See `connection-readiness.ts`.
  // Explicitly NOT `backendInfoQuery.dataUpdatedAt`: the `backend:info`
  // IPC always resolves regardless of remote reachability, so trusting
  // it dismisses the banner on a save-then-unreachable.
  const ready = !getPlatform().localHost
    ? wsStatus === "connected" &&
      savedAt !== null &&
      wsConnectedAtRef.current !== null &&
      wsConnectedAtRef.current >= savedAt
    : isDesktopServerReadyAfterSave({
        observation,
        wsStatus,
        wsConnectedAt: wsConnectedAtRef.current,
        savedAt,
      });
  useEffect(() => {
    if (banner?.dismissOnReady && ready) {
      setBanner(null);
      setSavedAt(null);
    }
  }, [banner, ready]);

  const classification = useMemo(() => {
    return validateServerUrlDraft(draft, { allowInsecureRemote });
  }, [allowInsecureRemote, draft]);

  const isUnchanged = draft.trim() === currentUrl.trim();
  // One source for "which owner receives this address, if any". `null`
  // means the deployment shape has not been observed yet, so there is
  // no command to send — the Save affordance and the submit handler
  // both read that same absence instead of each keeping their own copy
  // of the rule. The address field stays editable: what the user typed
  // is theirs to keep while the (sub-second, self-healing) snapshot
  // lands.
  const saveCommand = planServerUrlSave({
    mode: snapshot?.mode ?? null,
    serverUrl: draft.trim(),
  });
  const saveDisabled =
    save.isPending || classification.kind !== "ok" || isUnchanged || saveCommand === null;

  const onSubmit = (e: FormEvent): void => {
    e.preventDefault();
    if (saveDisabled || saveCommand === null) return;
    setBanner(null);
    setSavedAt(null);
    save.mutate(saveCommand);
  };

  const footer =
    classification.kind === "reject" || banner ? (
      <div className="flex flex-col gap-2">
        {classification.kind === "reject" ? (
          <p className="text-(length:--text-meta) text-error">{classification.message}</p>
        ) : null}
        {banner ? (
          <div
            role="status"
            className={cn(
              // Width from the theme's control cluster (poster = 2px) (#task:1833).
              "rounded-(--control-radius) border-(length:--control-border-width) px-3 py-2 text-xs",
              banner.tone === "error"
                ? "border-error bg-bg-inset text-error"
                : banner.tone === "success"
                  ? "border-success bg-bg-inset text-text-primary"
                  : "border-border-subtle bg-bg-inset text-text-secondary",
            )}
          >
            <div className="font-medium">{banner.title}</div>
            {banner.description ? (
              <div className="mt-1 text-text-muted">{banner.description}</div>
            ) : null}
          </div>
        ) : null}
      </div>
    ) : null;

  return (
    <SettingsPage>
      <SettingsSection label={t("appShell.server.label")} footer={footer}>
        <SettingsGroup>
          <ServerStatusRow status={connectionStatus} />

          <form
            data-settings-row
            onSubmit={onSubmit}
            className="flex min-w-0 flex-col gap-2 px-4 py-3"
          >
            <label
              htmlFor="settings-server-url"
              className="text-sm font-semibold text-text-primary"
            >
              {t("settings.connection.address")}
            </label>
            <div className="flex min-w-0 items-center gap-2">
              <Input
                id="settings-server-url"
                type="url"
                autoComplete="off"
                spellCheck={false}
                inputMode="url"
                placeholder="http://127.0.0.1:8765"
                value={draft}
                onChange={(e) => updateDraft(e.target.value)}
                disabled={save.isPending}
                className="h-8 min-w-0 flex-1 font-mono text-xs"
              />
              <Button
                type="submit"
                variant="primary"
                size="sm"
                className="shrink-0"
                disabled={saveDisabled}
                title={
                  classification.kind === "reject"
                    ? classification.message
                    : isUnchanged
                      ? t("appShell.unreachable.alreadySet")
                      : classification.kind === "empty"
                        ? t("appShell.unreachable.enterAddress")
                        : undefined
                }
              >
                {save.isPending ? (
                  <SpinnerIcon className="size-3.5 animate-spin" />
                ) : (
                  <SaveIcon className="size-3.5" />
                )}
                {t("common.save")}
              </Button>
            </div>
          </form>
        </SettingsGroup>
      </SettingsSection>
    </SettingsPage>
  );
}
