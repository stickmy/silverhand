import { ComputerStatusSkeleton } from "./computer-skeletons";
/**
 * `#task:808` slice 4 / `#task:809` polish — host detail / right pane.
 *
 * Identity-led layout that reuses the renderer's existing profile-card
 * vocabulary so this surface feels like part of the app instead of an
 * admin dashboard.
 *
 *   Header        — identity strip: tinted icon square as the
 *                   "avatar" (sized to balance the right-column stack
 *                   weight) plus a 3-row identity column:
 *                     1. InlineRename'able display label,
 *                     2. lightweight status (colored dot + uppercase
 *                        text) + Refresh button,
 *                     3. meta line (`host.name` only). Every computer
 *                        wears the same Monitor glyph — kind never swaps
 *                        the identity icon.
 *                   The state row sits in the middle so the visual
 *                   order reads "name → live state → identifiers"
 *                   (shiki dogfood `hLEODapuI_dHuCPz`).
 *   Computer      — one label-left / value-right list describing the
 *                   whole machine (IP, hostname, OS, memory, disk,
 *                   uptime), then the daemon's own facts as a quiet
 *                   footnote. Machine fields come from newer daemons
 *                   only; a missing field drops its row. Only rendered
 *                   when `availability === 'available'`. Offline, a
 *                   user-managed computer leads with Connect instead.
 *   Agents on     — `HostSection` titled "Agents on this machine"
 *   this machine    with the count rendered inline after the title.
 *                   Body: one-per-row list of agent identity cards.
 *                   Each card uses plain theme tokens
 *                   (`--border-subtle` at rest, `--text-primary` on
 *                   hover), NOT a louder framed-sticker treatment
 *                   that would be too heavy for information cards in
 *                   the detail pane (shiki dogfood `CEq57Kisu3w2d-Pl`).
 *
 * `HostSection` is a local primitive — solid `bg-bg-floating` white
 * tile with `border-radius: var(--floating-radius)` (poster = 0,
 * warm = `var(--radius-md)`) so the corners follow the theme's
 * floating-panel geometry. We deliberately do NOT reuse the shared
 * `ProfileSection` (which uses `bg-bg-floating/50`); shiki asked for
 * a heavier white treatment here and we don't want that ripple to
 * other profile surfaces.
 *
 * Forward-compat: NO `host.kind === 'local'` branching for server-backed
 * host status. shiki: "功能应是 hosts 主语的通用功能, 不是 local only" —
 * every server-backed screen here renders against generic `HostDto` +
 * `HostStatusResponse`.
 *
 * Exception: proxy config is daemon-host local truth. That form is only
 * editable when this Desktop can reach the physical daemon for `host.id`;
 * the server must not carry proxy bytes.
 *
 * Freshness contract: the shared status query stays a cache read
 * (`staleTime: Infinity`); this page re-probes the daemon once on open and on
 * Refresh via `useRefreshHostStatus`, which writes into the same key. Out-of-band attach/detach changes are reconciled
 * by the app-shell host connection sync, which invalidates this query from
 * viewer-scoped WS events / `snapshot_required`; no per-view poll loop needed.
 */
import { useQueryClient } from "@tanstack/react-query";
import {
  CheckIcon,
  ComputerIcon,
  CopyIcon,
  CpuIcon,
  DisconnectedIcon,
  RefreshIcon,
  SpinnerIcon,
  WarningIcon,
} from "@/icons";
import { useEffect, useMemo, useState, type JSX, type ReactNode } from "react";
import type {
  AgentOnHostDto,
  HostAdmissionFailure,
  HostDto,
  HostStatusResponse,
  RuntimeKind,
} from "@silverhand/shared";
import { reasoningEffortLabel, RuntimeKindSchema, runtimeLabel } from "@silverhand/shared";
import { AgentAvatar } from "@/components/agent-avatar";
import { AVATAR_PX } from "@/components/avatar-scale";
import { useAgentAvatarObjectUrls } from "@/features/agents/hooks/avatar-hooks";
import { useUsersById } from "@/features/users/hooks";
import { useUserAvatarObjectUrl } from "@/features/users/avatar-hooks";
import { UserAvatar } from "@/components/user-avatar";
import {
  isBackendUnreachable,
  isRegistrationRequired,
  isUpdateRequired,
  useViewerUser,
} from "@/features/session/hooks";
import { useNav } from "@/router";
import { Button } from "@/components/ui/button";
import { useCoarsePointer, useCompactLayout } from "@/navigation/layout";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { cn } from "@/lib/cn";
import { paneChrome } from "@/components/layout/pane-chrome";
import {
  hostStatusKey,
  useAgentOnlineByHost,
  useBackendInfoSnapshot,
  useHostStatusQuery,
  useRefreshHostStatus,
  useDisconnectHostMutation,
  useRotateConnectTokenMutation,
  useUpdateHostMutation,
} from "../../../features/computers/hooks/hooks";
import { computerActions } from "../model/computer-actions";
import {
  describeAdmissionFailure,
  describeHostStatus,
  formatGigabytes,
  formatUptime,
  type HostStatusTone,
} from "../model/host-status-presentation";
import { InlineRename } from "./inline-rename";
import { ConnectionCommandCard } from "./connection-command-card";
import { HostDiagnosticsAction } from "./host-diagnostics-action";
import { computerCopy } from "../../../features/computers/model/computer-copy";
import { formatOsShort } from "../model/format-os";
import { ProxySection } from "./host-proxy-section";
import {
  RefreshCredentialsSection,
  RevokeComputerSection,
  DeleteComputerSection,
} from "./host-danger-sections";
import { Banner, HostSection } from "./host-section";
import { t } from "@/lib/i18n";
import { toast } from "@/lib/toast";

/**
 * Lightweight status indicator — colored dot + uppercase text. shiki
 * dogfood `CEq57Kisu3w2d-Pl`: the prior filled HUD chip
 * (`bg-success text-text-on-success`) read as "heavy" next to a small
 * Refresh icon button — drop the solid tile, keep only the tone-colored
 * dot + muted caps so the state row stays a quiet secondary strip.
 *
 * The dot still carries the tone (success/warning/error/muted/accent)
 * — that's the at-a-glance signal — and the text stays uppercase
 * tracking-wider so it visually rhymes with the section card titles.
 */
const TONE_DOT_CLASS: Record<HostStatusTone, string> = {
  accent: "bg-accent",
  success: "bg-success",
  warning: "bg-warning",
  error: "bg-error",
  muted: "bg-text-muted/60",
};

export interface HostDetailViewProps {
  host: HostDto;
}

export function HostDetailView({ host }: HostDetailViewProps): JSX.Element {
  const qc = useQueryClient();
  const viewer = useViewerUser();
  const statusQuery = useHostStatusQuery(host.id);
  const renameMutation = useUpdateHostMutation(host.id);
  const reprobe = useRefreshHostStatus(host.id);
  const reprobeHost = reprobe.mutate;

  const refresh = () => {
    reprobe.mutate(undefined, {
      onError: () => void qc.invalidateQueries({ queryKey: hostStatusKey(host.id) }),
    });
  };

  // Machine usage is sampled when the daemon is asked, so opening the page
  // takes one fresh sample; the cached read renders meanwhile.
  useEffect(() => {
    reprobeHost();
  }, [host.id, reprobeHost]);

  const statusView = useMemo(
    () => (statusQuery.data ? describeHostStatus(statusQuery.data) : null),
    [statusQuery.data],
  );

  const trimmedLabel = host.display_label?.trim() ?? "";
  const showStableName = trimmedLabel.length > 0 && trimmedLabel !== host.name;
  const viewerState = viewer.data;
  const viewerId =
    viewerState &&
    !isRegistrationRequired(viewerState) &&
    !isBackendUnreachable(viewerState) &&
    !isUpdateRequired(viewerState)
      ? viewerState.id
      : null;
  const canEditHostLocalConfig = viewerId !== null && host.creator_user_id === viewerId;
  const backendInfo = useBackendInfoSnapshot();
  const currentLocalHostId = backendInfo.data?.local_host_id ?? null;
  // The mutex among Connection / Delete / Refresh / Revoke is derived
  // by one pure predicate (`computerActions`). See its docstring for
  // the ownership + lifecycle + provisioning-source rules and the
  // four-matrix test that locks them.
  const actions = computerActions({
    host,
    viewerId,
    currentLocalHostId,
  });

  return (
    <main
      data-panel="detail"
      className="flex h-full min-w-0 flex-1 flex-col overflow-y-auto"
      style={paneChrome}
    >
      <HostHero
        host={host}
        showStableName={showStableName}
        statusLabel={statusView?.label ?? null}
        statusTone={statusView?.tone ?? null}
        statusLoading={statusQuery.isLoading && !statusQuery.data}
        refreshing={statusQuery.isFetching || reprobe.isPending}
        onRefresh={() => void refresh()}
        onRename={async (next) => {
          await renameMutation.mutateAsync({ display_label: next });
        }}
        appManaged={host.provisioning_source === "app_install"}
      />

      <div className="mx-auto flex w-full max-w-2xl flex-col px-6">
        {statusQuery.isLoading ? (
          <div className="py-6">
            <ComputerStatusSkeleton />
          </div>
        ) : statusQuery.isError ? (
          <div className="py-6">
            <Banner tone="error">
              {t("computers.couldNotLoadThis")}
              {statusQuery.error instanceof Error ? ` (${statusQuery.error.message})` : ""}
            </Banner>
          </div>
        ) : statusQuery.data ? (
          <>
            {statusQuery.data.availability === "disconnected" &&
            statusQuery.data.admission_failure ? (
              <AdmissionFailureNotice
                failure={statusQuery.data.admission_failure}
                appManaged={host.provisioning_source === "app_install"}
              />
            ) : null}
            {statusQuery.data.availability === "available" ? (
              <ComputerSection
                snapshot={statusQuery.data.snapshot}
                createdAt={host.created_at}
                creatorUserId={host.creator_user_id}
              />
            ) : actions.connection ? (
              <ConnectionSection host={host} status={statusQuery.data} />
            ) : (
              <OfflineSection status={statusQuery.data} />
            )}
            <RegisteredAgentsSection hostId={host.id} agents={statusQuery.data.agents} />
            {canEditHostLocalConfig ? (
              <ProxySection hostId={host.id} availability={statusQuery.data.availability} />
            ) : null}
            {actions.connection && statusQuery.data.availability === "available" ? (
              <ConnectionSection host={host} status={statusQuery.data} />
            ) : null}
            {actions.refresh ? <RefreshCredentialsSection host={host} /> : null}
          </>
        ) : null}
        {/* Footer action group (#task:1619, shiki 2026-07-26). `Copy
            diagnostics` and the one terminal verb this computer allows are ONE
            group: full-width buttons, a single divider above the group, none
            between them. They used to be two sibling `<section>`s, so the
            divider + double `py-6` read as two unrelated pages of settings.
            Diagnostics still lives outside the status-conditional branches
            above — offline / unreachable / status-load-failed is exactly when a
            user reaches for it — and `revoke` / `delete` are mutually exclusive
            per `computerActions`, so this group is never more than two rows. */}
        <section className="flex flex-col gap-2 border-t border-border-subtle py-6 first:border-t-0">
          <HostDiagnosticsAction host={host} />
          {actions.revoke ? <RevokeComputerSection host={host} /> : null}
          {actions.delete ? <DeleteComputerSection host={host} /> : null}
        </section>
      </div>
    </main>
  );
}

/* ─── Connection (user-managed connect / disconnect) ────────────── */

/**
 * #task:1348 — the target-page Connection section for a user-managed
 * computer. Deliberately minimal (shiki): the user only needs to know how
 * to connect and how to disconnect — no rules prose. Two buttons:
 *
 *   - **Generate connection command** (rotate the credential) — mints a
 *     fresh reveal-once command; the backend invalidates the previous token
 *     and force-drops any machine currently using it. The connect /
 *     re-pair / lost-command / move-machine path.
 *   - **Disconnect** — credential-preserving: drops the live WS without
 *     touching the token, so the same command reconnects. Shown ONLY while
 *     the computer is actually connected (`availability === 'available'`);
 *     nothing to drop otherwise. It is "disconnect for now", not a
 *     permanent off-switch — if the machine itself restarts it reconnects
 *     with its still-valid credential. Permanently removing a computer is
 *     Delete, a computer-level action that lives OUTSIDE this section
 *     (#task:1354).
 *
 * The rotate cleartext is reveal-once: the `connect_token` lands in local
 * state (`minted`) and the shared `ConnectionCommandCard` assembles the command
 * locally (#task:1428) from it + this desktop's connected server URL. Both are
 * discarded on unmount — never cached, never logged.
 */
function ConnectionSection({
  host,
  status,
}: {
  host: HostDto;
  status: HostStatusResponse;
}): JSX.Element {
  const availability = status.availability;
  const rotate = useRotateConnectTokenMutation(host.id);
  const disconnect = useDisconnectHostMutation(host.id);
  const compact = useCompactLayout();
  // Offline, this section leads the page, so it carries the status detail
  // that `OfflineSection` would otherwise show.
  const offlineDetail = availability === "available" ? null : describeHostStatus(status).detail;
  // #task:1428 — the command is assembled locally from this desktop's own
  // connected server URL, never the server-echoed `server_url`.
  const backendInfo = useBackendInfoSnapshot();
  const [minted, setMinted] = useState<{
    daemonNpmSpec: string;
    connectToken: string;
  } | null>(null);
  const [rotateError, setRotateError] = useState(false);
  const [disconnectError, setDisconnectError] = useState(false);

  const handleGenerate = async (): Promise<void> => {
    setRotateError(false);
    try {
      const res = await rotate.mutateAsync();
      setMinted({
        daemonNpmSpec: res.daemon_npm_spec,
        connectToken: res.connect_token,
      });
    } catch {
      setRotateError(true);
    }
  };

  const handleDisconnect = async (): Promise<void> => {
    setDisconnectError(false);
    try {
      await disconnect.mutateAsync();
      // On success availability flips to `disconnected` → the button hides.
    } catch {
      setDisconnectError(true);
    }
  };

  return (
    <HostSection title={computerCopy().connectionSectionTitle}>
      <div className="flex flex-col gap-3">
        {offlineDetail ? <p className="text-sm text-text-muted">{offlineDetail}</p> : null}
        {minted ? (
          <ConnectionCommandCard
            daemonNpmSpec={minted.daemonNpmSpec}
            connectToken={minted.connectToken}
            connectedServerUrl={backendInfo.data?.url ?? ""}
          />
        ) : null}

        <div className="flex flex-col gap-2">
          <Button
            size={compact ? "md" : "sm"}
            variant="primary"
            className="w-full gap-2"
            onClick={() => void handleGenerate()}
            disabled={rotate.isPending}
          >
            {rotate.isPending ? (
              <SpinnerIcon className="size-3.5 animate-spin" />
            ) : (
              <RefreshIcon className="size-3.5" />
            )}
            {computerCopy().generateCommand}
          </Button>

          {availability === "available" ? (
            <Button
              size={compact ? "md" : "sm"}
              variant="neutral"
              className="w-full gap-2"
              onClick={() => void handleDisconnect()}
              disabled={disconnect.isPending}
            >
              {disconnect.isPending ? (
                <SpinnerIcon className="size-3.5 animate-spin" />
              ) : (
                <DisconnectedIcon className="size-3.5" />
              )}
              {computerCopy().disconnectCurrent}
            </Button>
          ) : null}
        </div>

        {rotateError ? <p className="text-xs text-error">{computerCopy().rotateFailed}</p> : null}
        {disconnectError ? (
          <p className="text-xs text-error">{computerCopy().disconnectFailed}</p>
        ) : null}
      </div>
    </HostSection>
  );
}

/* ─── Hero ──────────────────────────────────────────────────────── */

function HostHero({
  host,
  showStableName,
  statusLabel,
  statusTone,
  statusLoading,
  refreshing,
  onRefresh,
  onRename,
  appManaged,
}: {
  host: HostDto;
  showStableName: boolean;
  statusLabel: string | null;
  statusTone: HostStatusTone | null;
  statusLoading: boolean;
  refreshing: boolean;
  onRefresh: () => void;
  onRename: (next: string | null) => Promise<void>;
  /** Provenance chip in the state row. This is lifecycle/origin
   *  metadata (per doc §3.1), NOT a capability signal — the four
   *  danger surfaces below are picked by `computerActions`, not by
   *  this flag. */
  appManaged: boolean;
}): JSX.Element {
  return (
    <header className="flex shrink-0 items-start gap-4 border-b border-border-subtle bg-bg-floating px-6 py-4">
      <HostBadge />

      <div className="flex min-w-0 flex-1 flex-col gap-1.5">
        <div className="font-serif text-lg font-semibold leading-tight text-text-primary">
          <InlineRename
            value={host.display_label}
            placeholder={host.name}
            fallbackForEdit={host.display_label ?? host.name}
            ariaLabel={t("computers.renameThisComputer")}
            onCommit={onRename}
          />
        </div>

        <div className="flex items-center gap-2">
          {statusLabel && statusTone ? (
            <StatusChip label={statusLabel} tone={statusTone} />
          ) : statusLoading ? (
            <span className="inline-flex items-center gap-1.5 border border-transparent px-1 py-0.5 text-(length:--text-badge) text-text-muted">
              <SpinnerIcon className="size-3.75 animate-spin" aria-hidden />
              <span>{t("computers.loading")}</span>
            </span>
          ) : null}
          {appManaged ? <ProvenanceChip /> : null}
          <Button
            variant="ghost"
            size="icon"
            // Drop the default `size="icon"` (h-8 w-8 p-0). Use the
            // same border + py-0.5 construction as the chip so the
            // button height tracks the chip's text-derived height
            // automatically — same anatomy, swapped fill (the chip's
            // visible border becomes a transparent placeholder here so
            // padding alignment is identical).
            className="h-auto w-auto border border-transparent px-1 py-0.5"
            onClick={onRefresh}
            disabled={refreshing}
            aria-label={t("computers.refreshHostStatus")}
            title={t("computers.refresh")}
          >
            <RefreshIcon className={cn("size-3.75", refreshing && "animate-spin")} />
          </Button>
        </div>
        {/* Meta line: stable `host.name` (mono) ONLY when the user has
            renamed the host AND the rename actually differs from the
            underlying name. Otherwise the title (which falls back to
            `host.name` itself) would repeat the same string here —
            "two local" déjà-vu (shiki `CEq57Kisu3w2d-Pl` /
            Cindy `Fjg86GQM5qwoEjl7`). */}
        {showStableName ? (
          <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-text-muted">
            <span className="font-mono">{host.name}</span>
          </div>
        ) : null}
      </div>
    </header>
  );
}

/**
 * Host "avatar" analog. Mirrors `AgentAvatar`'s tinted square so the
 * Hero reads as identity-led (same silhouette as Members/Profile
 * panels), without inventing a host-specific decoration.
 *
 * Sized to roughly match the right-column stack height (title + state
 * row + meta) so the icon carries visual weight equal to the textual
 * identity instead of looking dwarfed beside the lg/serif title —
 * shiki dogfood `hLEODapuI_dHuCPz`.
 */
function HostBadge(): JSX.Element {
  // Every computer wears the same Monitor glyph — provenance/availability
  // is conveyed elsewhere, never by swapping the identity icon (shiki
  // dogfood: a network glyph made user-added computers read as a different
  // kind of thing). Kept in sync with the sidebar row's icon rule.
  const Icon = ComputerIcon;
  return (
    <span
      data-host-hero
      data-sticker
      data-sticker-tone="host"
      data-sticker-no-frame
      data-sticker-size="lg"
      className="inline-flex size-20 shrink-0 items-center justify-center rounded-none bg-bg-elevated text-text-secondary"
      aria-hidden
    >
      <Icon className="size-9" />
    </span>
  );
}

function StatusChip({ label, tone }: { label: string; tone: HostStatusTone }): JSX.Element {
  return (
    <span
      role="status"
      data-panel-pill="host-status"
      className="inline-flex items-center gap-1.5 border border-border-subtle bg-transparent px-2 py-0.5 text-(length:--text-badge) font-semibold uppercase tracking-wider text-text-secondary"
    >
      <span
        data-status-pip
        className={cn("size-1.5 rounded-(--pill-radius)", TONE_DOT_CLASS[tone])}
        aria-hidden
      />
      {label}
    </span>
  );
}

/**
 * Provenance chip — sibling of `StatusChip` in geometry, distinct in
 * meaning. This is lifecycle/origin metadata (per doc §3.1: "explains
 * where a host came from; drives nothing runtime"), never a capability
 * signal. The four danger surfaces are picked by `computerActions`,
 * not by anything a viewer can read here.
 */
function ProvenanceChip(): JSX.Element {
  return (
    <span
      data-panel-pill="host-provenance"
      className="inline-flex items-center border border-border-subtle bg-transparent px-2 py-0.5 text-(length:--text-badge) font-semibold uppercase tracking-wider text-text-muted"
    >
      {computerCopy().appManaged}
    </span>
  );
}

/* ─── Computer (available) ────────────────────────────────────── */

type HostSnapshot = Extract<HostStatusResponse, { availability: "available" }>["snapshot"];

/** At or above this share a usage bar turns warning: the one reading on this
 *  page that asks the user to act. */
const USAGE_WARNING_RATIO = 0.9;

function ComputerSection({
  snapshot,
  createdAt,
  creatorUserId,
}: {
  snapshot: HostSnapshot;
  createdAt: number;
  creatorUserId: string | null;
}): JSX.Element {
  return (
    <HostSection title={t("computers.computer")}>
      <TooltipProvider>
        <dl data-host-system-list className="flex flex-col gap-2.5">
          {snapshot.ip ? (
            <SystemField label={t("computers.ipAddress")}>
              <CopyableValue value={snapshot.ip} />
            </SystemField>
          ) : null}
          {snapshot.hostname ? (
            <SystemField label={t("computers.hostname")}>
              <span className="text-text-primary">{snapshot.hostname}</span>
            </SystemField>
          ) : null}
          <SystemField label={t("computers.os")}>
            <span className="text-text-primary">{formatOsShort(snapshot.os)}</span>
            {snapshot.os_arch ? (
              <span className="ml-2 text-text-muted">{snapshot.os_arch}</span>
            ) : null}
          </SystemField>
          <UsageField
            label={t("computers.memory")}
            used={snapshot.memory_used_bytes}
            total={snapshot.memory_total_bytes}
            base={1024}
          />
          <UsageField
            label={t("computers.disk")}
            used={snapshot.disk_used_bytes}
            total={snapshot.disk_total_bytes}
            base={1000}
          />
          {snapshot.system_uptime_ms !== undefined ? (
            <SystemField label={t("computers.uptime")}>
              <span className="text-text-primary">{formatUptime(snapshot.system_uptime_ms)}</span>
            </SystemField>
          ) : null}
        </dl>
      </TooltipProvider>
      <dl
        data-host-system-list
        className="mt-4 flex flex-col gap-1.5 border-t border-dashed border-border-subtle pt-4 text-xs text-text-muted"
      >
        <SystemField label={t("computers.daemon")} quiet>
          {snapshot.daemon_version}
          <span className="ml-2">
            {t("computers.daemonProcess", {
              pid: snapshot.daemon_pid,
              uptime: formatUptime(snapshot.uptime_ms),
            })}
          </span>
        </SystemField>
        <SystemField label={t("computers.added")} quiet>
          {formatCreatedAt(createdAt)}
        </SystemField>
        {creatorUserId ? <CreatorField userId={creatorUserId} /> : null}
      </dl>
    </HostSection>
  );
}

/** Who added the computer, resolved from the global user directory so a
 *  rename or new avatar shows here without a host refetch. Unresolved
 *  (directory still loading) drops the row rather than printing a raw id. */
function CreatorField({ userId }: { userId: string }): JSX.Element | null {
  const usersById = useUsersById();
  const avatarObjectUrl = useUserAvatarObjectUrl({ userId });
  const name = usersById.get(userId)?.display_name ?? null;

  if (!name) {
    return null;
  }

  return (
    <SystemField label={t("computers.addedBy")} quiet>
      <span className="inline-flex items-center gap-1.5 align-middle">
        <UserAvatar size={AVATAR_PX.xs} avatarObjectUrl={avatarObjectUrl} alt={name} />
        {name}
      </span>
    </SystemField>
  );
}

/** Memory and disk share one reading: a short bar and the share. The amounts
 *  live in a tooltip (tap on touch). Memory counts in binary units and disk in
 *  decimal ones, matching how each OS reports them. */
function UsageField({
  label,
  used,
  total,
  base,
}: {
  label: string;
  used: number | undefined;
  total: number | undefined;
  base: 1000 | 1024;
}): JSX.Element | null {
  const coarse = useCoarsePointer();
  const [tapped, setTapped] = useState(false);

  if (used === undefined || total === undefined || total <= 0) {
    return null;
  }

  const ratio = Math.min(1, Math.max(0, used / total));
  const hot = ratio >= USAGE_WARNING_RATIO;
  const detail = t("computers.usageDetail", {
    used: formatGigabytes(used, base),
    total: formatGigabytes(total, base),
  });

  return (
    <SystemField label={label}>
      <Tooltip open={coarse ? tapped : undefined} onOpenChange={coarse ? setTapped : undefined}>
        <TooltipTrigger asChild onClick={coarse ? () => setTapped(true) : undefined}>
          <span
            tabIndex={0}
            aria-label={`${label}: ${detail}`}
            className="inline-flex items-center gap-3 outline-none"
          >
            <span
              aria-hidden
              className="relative h-1.5 w-16 shrink-0 bg-bg-elevated [[data-layout=regular]_&]:w-28"
            >
              <span
                className={cn("absolute inset-y-0 left-0", hot ? "bg-warning" : "bg-text-primary")}
                style={{ width: `${ratio * 100}%` }}
              />
            </span>
            <span className={hot ? "font-semibold text-warning" : "text-text-primary"}>
              {Math.round(ratio * 100)}%
            </span>
          </span>
        </TooltipTrigger>
        <TooltipContent>{detail}</TooltipContent>
      </Tooltip>
    </SystemField>
  );
}

/** A value the user takes elsewhere (an address for screen sharing or SSH).
 *  Copy confirms in place; no toast. */
function CopyableValue({ value }: { value: string }): JSX.Element {
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    if (!copied) {
      return;
    }
    const timer = window.setTimeout(() => setCopied(false), 1500);
    return () => window.clearTimeout(timer);
  }, [copied]);

  const handleCopy = async (): Promise<void> => {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(true);
    } catch {
      toast.error(t("common.copyFailed"));
    }
  };

  return (
    <button
      type="button"
      onClick={() => void handleCopy()}
      aria-label={t("computers.copyValue", { value })}
      className="group inline-flex items-center gap-2 text-text-primary"
    >
      {value}
      {copied ? (
        <CheckIcon className="size-3.5 text-success" aria-hidden />
      ) : (
        <CopyIcon
          className="size-3.5 text-text-muted opacity-60 transition-opacity group-hover:opacity-100"
          aria-hidden
        />
      )}
    </button>
  );
}

/** Host creation date as `YYYY-MM-DD`. Built from local calendar parts,
 *  zero-padded — a lifecycle date, no time of day. */
function formatCreatedAt(createdAt: number): string {
  const d = new Date(createdAt);
  const month = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${d.getFullYear()}-${month}-${day}`;
}

function SystemField({
  label,
  quiet = false,
  children,
}: {
  label: string;
  quiet?: boolean;
  children: ReactNode;
}): JSX.Element {
  return (
    <div className="flex items-baseline gap-4">
      <dt className="w-28 shrink-0 text-(length:--text-meta) font-medium uppercase tracking-wider text-text-muted">
        {label}
      </dt>
      <dd
        data-host-system-value
        className={cn("min-w-0 flex-1 tabular-nums wrap-anywhere", quiet ? "text-xs" : "text-sm")}
      >
        {children}
      </dd>
    </div>
  );
}

/* ─── Offline arms ──────────────────────────────────────────────── */

/**
 * #task:1631 — the "this one needs attention" notice on a computer page.
 *
 * shiki interaction gate (2026-07-26): an incompatible-version prompt for
 * a computer is **not** a banner — it is a notice placed on that
 * computer's page, pale-yellow fill + icon. So it reuses the established
 * caution-callout vocabulary of this file (`--warning-surface` fill +
 * `--control-radius`, the same form as `CascadeImpact`) instead of the
 * status `Banner`, and the status
 * chrome keeps saying plain "Offline" — one voice per question: the pill
 * answers "is it connected", this answers "what do I do about it".
 *
 * `AlertTriangle`, deliberately not an update/download glyph: the only
 * fix runs on that machine, so an icon that reads like an in-app update
 * affordance would promise a button that does not exist.
 *
 * `appManaged` selects which artifact the action clause names (the app
 * itself vs the published package the connection command installs) — see
 * `describeAdmissionFailure`.
 */
function AdmissionFailureNotice({
  failure,
  appManaged,
}: {
  failure: HostAdmissionFailure;
  appManaged: boolean;
}): JSX.Element {
  return (
    <div
      role="status"
      className="mt-5 flex items-start gap-2.5 rounded-(--control-radius) bg-(--warning-surface) px-3 py-2.5"
    >
      <WarningIcon className="mt-0.5 size-4 shrink-0 text-warning" aria-hidden />
      <p className="min-w-0 flex-1 text-sm text-text-primary">
        {describeAdmissionFailure(failure, { appManaged })}
      </p>
    </div>
  );
}

function OfflineSection({
  status,
}: {
  status: Exclude<HostStatusResponse, { availability: "available" }>;
}): JSX.Element {
  const view = describeHostStatus(status);
  // No generic "will appear when the computer is back online" fallback:
  // every non-available arm already carries its own detail, and one of
  // them (`disconnected` + `admission_failure`, `#task:1631`) is offline
  // precisely BECAUSE it will not come back unattended — printing a
  // wait-and-see promise there would be the one wrong thing to say.
  return (
    <HostSection title={t("computers.computer")}>
      <Banner tone={view.tone}>
        <span className="font-medium">{view.label}.</span> {view.detail}
      </Banner>
    </HostSection>
  );
}

/* ─── Registered agents ─────────────────────────────────────────── */

function RegisteredAgentsSection({
  hostId,
  agents,
}: {
  hostId: string;
  agents: AgentOnHostDto[];
}): JSX.Element {
  const nav = useNav();
  // Agent-on-computer liveness (layer-2) is the SAME fact the rail dot
  // reads. Read it from the shared `useAgentOnlineByHost` (same
  // `hostStatusKey`, event-driven: the backend pushes
  // `host_agent_liveness_changed` invalidations — #task:1588 removed
  // the old 5s poll) so the pip stays single-source with the rail dot.
  // Falls back to the frozen snapshot's `online` until the first live
  // sample lands. Do NOT source layer-3 conversation status here —
  // this list's subject is the computer.
  const liveOnlineByHost = useAgentOnlineByHost([hostId]);
  // shiki dogfood `DXVl5tEBshVT2LqF` #2: route avatar uploads through
  // the shared object-URL cache so cards show the user-picked image
  // instead of the runtime tint fallback. Memoize the source list so
  // the batch hook's cache key stays stable across renders.
  const avatarSources = useMemo(
    () => agents.map((a) => ({ id: a.agent_id, avatar_url: a.avatar_url })),
    [agents],
  );
  const avatarUrls = useAgentAvatarObjectUrls(avatarSources);
  return (
    <HostSection
      title={
        <span className="inline-flex items-baseline gap-2">
          <span>{t("computers.agentsOnThisMachine")}</span>
          <span className="text-text-muted">{agents.length}</span>
        </span>
      }
    >
      {agents.length === 0 ? (
        <p className="text-sm text-text-muted">{t("computers.noAgentsAreRegistered")}</p>
      ) : (
        <ul className="flex flex-col gap-2" aria-label={t("computers.agentsOnThisMachine")}>
          {agents.map((agent) => {
            const live = liveOnlineByHost.get(agent.agent_id);
            const online: "online" | "offline" =
              live === undefined ? agent.online : live ? "online" : "offline";
            return (
              <AgentCard
                key={agent.agent_id}
                agent={agent}
                online={online}
                avatarObjectUrl={avatarUrls.get(agent.agent_id) ?? null}
                onOpen={() => nav.toMembers(agent.agent_id)}
              />
            );
          })}
        </ul>
      )}
    </HostSection>
  );
}

/**
 * Per-agent identity card. Reuses `AgentAvatar` (with the user-
 * uploaded `avatarObjectUrl` when available) so the host detail
 * surface speaks the same identity vocab as #Members / Chat. Status
 * reads as a dot+text pip aligned with the Hero status indicator.
 *
 * **Border / radius / hover** (shiki dogfood `DXVl5tEBshVT2LqF` #1):
 * uses plain theme tokens — `border-border-subtle` at rest,
 * transitions to `border-text-primary` on hover — with width driven
 * by `var(--control-border-width)` (poster = 2px, warm = 1px) so the
 * card chrome matches the theme's stroke weight. Radius is NOT forced
 * here: poster wants 0 (poster aesthetic) and the browser default is
 * 0, which is what we get without inline radius styling. The previous
 * `var(--card-radius)` (poster = 6px) was wrong — shiki only asked
 * for a color change, not a corner change.
 *
 * **Click**: the whole card is a button that navigates to
 * `routes.membersAgent(id)` — same destination as the #Members tab
 * uses. `<li>` wrapping a button keeps the list semantics intact while
 * the inner button takes the focus/click handlers.
 *
 * `AgentOnHostDto.runtime` is `string` on the wire (forward-compat for
 * future runtimes daemon-side). We narrow via `RuntimeKindSchema.safeParse`;
 * a runtime the renderer doesn't know yet renders a generic
 * `<Cpu />` tile rather than borrowing a brand color we don't own.
 */
function AgentCard({
  agent,
  online,
  avatarObjectUrl,
  onOpen,
}: {
  agent: AgentOnHostDto;
  online: "online" | "offline";
  avatarObjectUrl: string | null;
  onOpen: () => void;
}): JSX.Element {
  const knownRuntime = RuntimeKindSchema.safeParse(agent.runtime);
  return (
    <li>
      <button
        type="button"
        onClick={onOpen}
        aria-label={t("computers.openProfile", { name: agent.display_name })}
        data-host-agent-card
        className="flex w-full items-center gap-3 rounded-(--row-radius) border-(length:--control-border-width) border-border-subtle bg-bg-floating px-3 py-2.5 text-left transition-colors hover:border-text-primary focus-visible:border-text-primary focus-visible:outline-none"
      >
        {knownRuntime.success ? (
          <AgentAvatar
            runtime={knownRuntime.data as RuntimeKind}
            size={AVATAR_PX.xl}
            avatarObjectUrl={avatarObjectUrl}
            alt={agent.display_name}
          />
        ) : (
          <span
            className="inline-flex size-9 shrink-0 items-center justify-center rounded-none bg-bg-elevated text-text-muted"
            aria-hidden
            title={t("computers.unknownRuntime", { runtime: agent.runtime })}
          >
            <CpuIcon className="size-5" />
          </span>
        )}
        <div className="flex min-w-0 flex-1 flex-col gap-0.5">
          <span className="flex min-w-0 items-baseline gap-2">
            <span className="truncate text-sm font-semibold text-text-primary">
              {agent.display_name}
            </span>
            {/* `handle` is already `@<name>` per backend DTO contract —
             *  do NOT prefix here or the UI shows `@@<name>`. */}
            <span className="truncate font-mono text-xs text-text-muted">{agent.handle}</span>
          </span>
          <AgentConfigLine agent={agent} />
        </div>
        <AgentStatusPip online={online} />
      </button>
    </li>
  );
}

/** Runtime, then the configuration the agent runs with. Unset values are left
 *  out rather than labelled as defaults: a backend that predates these fields
 *  reports every one as null, which would mislabel configured agents. Parts
 *  are separated by space, not a glyph: model ids carry their own `/`. */
function AgentConfigLine({ agent }: { agent: AgentOnHostDto }): JSX.Element {
  const knownRuntime = RuntimeKindSchema.safeParse(agent.runtime);
  const parts = [
    knownRuntime.success ? runtimeLabel(knownRuntime.data) : agent.runtime,
    agent.model,
    agent.reasoning_effort ? reasoningEffortLabel(agent.reasoning_effort) : null,
    agent.fast_mode ? t("computers.fast") : null,
  ].filter((part): part is string => part !== null);

  return (
    <span className="flex min-w-0 items-baseline gap-x-3 overflow-hidden text-(length:--text-meta) text-text-muted">
      {parts.map((part, index) => (
        <span
          key={index}
          className={cn(
            index === 1 ? "min-w-0 truncate" : "shrink-0",
            index === 0 && "font-semibold",
          )}
        >
          {part}
        </span>
      ))}
    </span>
  );
}

/**
 * Per-agent online indicator. Same dot-plus-text form as the Hero
 * `StatusChip` — drop the prior filled `bg-success` pill so the cards
 * stay quiet on hover/sweep and the green dot does the at-a-glance
 * signaling (shiki dogfood `CEq57Kisu3w2d-Pl`: "色彩使用注意主题搭配").
 */
function AgentStatusPip({ online }: { online: "online" | "offline" }): JSX.Element {
  return (
    <span
      role="status"
      aria-label={online === "online" ? t("computers.online") : t("computers.offline")}
      className="inline-flex items-center gap-1.5 text-(length:--text-meta) font-medium uppercase tracking-wider text-text-muted"
    >
      <span
        data-status-pip
        className={cn(
          "size-1.5 rounded-(--pill-radius)",
          online === "online" ? "bg-success" : "bg-text-muted/40",
        )}
        aria-hidden
      />
      {/* Phone width keeps the dot only, so the config line keeps its room. */}
      <span className="[[data-layout=compact]_&]:hidden">
        {online === "online" ? t("computers.online") : t("computers.offline")}
      </span>
    </span>
  );
}
