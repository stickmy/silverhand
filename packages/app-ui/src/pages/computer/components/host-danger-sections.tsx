import { CpuIcon, DeleteIcon, RefreshIcon, ShieldOffIcon, SpinnerIcon, SuccessIcon } from "@/icons";
import { useState, type JSX } from "react";
import type { HostAgentResource, HostDto, RuntimeKind } from "@silverhand/shared";
import { RuntimeKindSchema } from "@silverhand/shared";
import { AgentAvatar } from "@/components/agent-avatar";
import { AVATAR_PX } from "@/components/avatar-scale";
import { Button } from "@/components/ui/button";
import { useCompactLayout } from "@/navigation/layout";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  useHostResourcesQuery,
  useDeleteComputerMutation,
  useRevokeComputerMutation,
  useRotateHostTokenMutation,
} from "../../../features/computers/hooks/hooks";
import {
  resolveDeleteComputerPhase,
  resolveDeleteFailure,
  type DeleteComputerFailure,
} from "../model/delete-computer-phase";
import { computerCopy } from "../../../features/computers/model/computer-copy";
import { HostSection } from "./host-section";
import { t } from "@/lib/i18n";

/* ─── Refresh credentials (app-managed, current-machine only) ──── */

/**
 * The app-managed counterpart to `ConnectionSection`. Rotates the
 * backend-issued host token in local daemon-config via a main-process
 * IPC (`requireLocalHost().rotateHostToken`) — token bytes NEVER enter
 * the renderer. Deliberately NOT merged with `ConnectionSection`: the
 * two target different daemon layers (local config vs. server-issued
 * connect-token) and their copy is different.
 *
 * Gated (by `computerActions`) to `host.id === currentLocalHostId`
 * because the mutation writes to THIS desktop's daemon config; trying
 * to rotate another machine's app-managed token from here would
 * silently miss the daemon that actually reads it.
 */
export function RefreshCredentialsSection({ host }: { host: HostDto }): JSX.Element {
  const rotate = useRotateHostTokenMutation(host.id);
  const compact = useCompactLayout();
  const [saved, setSaved] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const handleRefresh = async (): Promise<void> => {
    setError(null);
    setSaved(false);
    try {
      await rotate.mutateAsync();
      setSaved(true);
      window.setTimeout(() => setSaved(false), 1500);
    } catch (err) {
      setError(err instanceof Error ? err.message : computerCopy().refreshCredentialsFailed);
    }
  };

  return (
    <HostSection title={computerCopy().refreshCredentialsTitle}>
      <div className="flex flex-col gap-3">
        <p className="text-sm text-text-muted">{computerCopy().refreshCredentialsBody}</p>
        <div className="flex flex-col gap-2">
          <Button
            size={compact ? "md" : "sm"}
            variant="primary"
            className="w-full gap-2"
            onClick={() => void handleRefresh()}
            disabled={rotate.isPending}
          >
            {rotate.isPending ? (
              <SpinnerIcon className="size-3.5 animate-spin" />
            ) : (
              <RefreshIcon className="size-3.5" />
            )}
            {rotate.isPending
              ? computerCopy().refreshingCredentials
              : computerCopy().refreshCredentials}
          </Button>
          {saved ? (
            <span className="inline-flex items-center gap-1.5 text-xs text-success">
              <SuccessIcon className="size-3.5" />
              {computerCopy().refreshCredentialsSuccess}
            </span>
          ) : null}
        </div>
        {error ? <p className="text-xs text-error">{error}</p> : null}
      </div>
    </HostSection>
  );
}

/* ─── Revoke (app-managed, soft) ────────────────────────────────── */

/**
 * The app-managed soft-revoke surface. Distinct from
 * `DeleteComputerSection` (user-managed terminal delete): revoke sets
 * `revoked_at`, invalidates the credential, and force-drops the live
 * WS while preserving agents and workspaces (doc §5.3). Different
 * component, different mutation, different confirm copy — the two
 * never share a backend verb.
 *
 * Visually the trigger is the SAME red poster sticker as Delete (shiki
 * 2026-07-26: "Revoke Computer 按钮也改成这个样式"). Ranking the two verbs
 * by chrome was the earlier read and it was wrong: both are terminal for
 * this computer's connection, they are mutually exclusive (a computer
 * offers one or the other, never both), so a user never sees them side by
 * side to compare. The difference that matters — reversible vs permanent,
 * what survives — is carried by the verb and by the confirm copy, not by a
 * quieter frame.
 *
 * Revoking the current local computer does NOT log the user out.
 * User sessions are user-level and outlive Computer lifecycle; the
 * detail view flips to the revoked-terminal read-only shape (via the
 * invalidated queries) and the user can add another computer or keep
 * using other daemon-connected computers.
 */
export function RevokeComputerSection({ host }: { host: HostDto }): JSX.Element {
  const revoke = useRevokeComputerMutation(host.id);
  const [open, setOpen] = useState(false);
  const [failed, setFailed] = useState(false);

  const openConfirm = (): void => {
    setFailed(false);
    setOpen(true);
  };

  const handleRevoke = async (): Promise<void> => {
    setFailed(false);
    try {
      await revoke.mutateAsync();
      setOpen(false);
    } catch {
      setFailed(true);
    }
  };

  const label = host.display_label?.trim() || host.name;

  return (
    <>
      <Button variant="destructive-outline" className="gap-2 w-full" onClick={openConfirm}>
        <ShieldOffIcon className="size-3.5" />
        {computerCopy().revokeComputer}
      </Button>

      <Dialog
        open={open}
        onOpenChange={(next) => {
          setOpen(next);
          if (!next) setFailed(false);
        }}
      >
        <DialogContent className="max-w-md">
          <DialogHeader>
            <DialogTitle>{computerCopy().revokeConfirmTitle}</DialogTitle>
            <DialogDescription asChild>
              <ul className="mt-1 flex list-disc flex-col gap-1 pl-4">
                {computerCopy().revokeConfirmFacts.map((fact) => (
                  <li key={fact}>{fact}</li>
                ))}
              </ul>
            </DialogDescription>
          </DialogHeader>
          {failed ? <p className="text-xs text-error">{computerCopy().revokeFailed}</p> : null}
          <DialogFooter>
            <Button variant="cancel" onClick={() => setOpen(false)} disabled={revoke.isPending}>
              {computerCopy().cancel}
            </Button>
            <Button
              variant="destructive"
              className="gap-2"
              onClick={() => void handleRevoke()}
              disabled={revoke.isPending}
              aria-label={`${computerCopy().revokeComputer}: ${label}`}
            >
              {revoke.isPending ? (
                <SpinnerIcon className="size-3.5 animate-spin" />
              ) : (
                <ShieldOffIcon className="size-3.5" />
              )}
              {revoke.isPending ? computerCopy().revoking : computerCopy().revokeConfirm}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}

/* ─── Delete (computer-level removal) ───────────────────────────── */

/**
 * #task:1354 / #task:1380 — the single computer-level removal verb. Deliberately
 * lives OUTSIDE the Connection section (shiki: "delete 不应该在 connection 中,在别
 * 的地方") as a quiet danger action at the page footer, not a loud red box —
 * the error tone + the confirm dialog carry the warning, so the trigger stays
 * an outline button (the saturated fill is reserved for the actual commit
 * inside the dialog).
 *
 * TWO STEPS, not a discovery loop (shiki 2026-07-25): opening the dialog reads
 * `GET /hosts/:id/resources` — the ordinary "what is rooted on this computer"
 * query — shows the impact, and takes ONE confirm that sends
 * `cascade_dependents: true`. The structural branch is decided by
 * `resolveDeleteComputerPhase` (pure, unit-tested) off that read, BEFORE anything
 * destructive is offered; this component only renders what it returns:
 *   - **loading** — the standard confirm screen with Delete disabled. The impact
 *     must be on screen before the button can be pressed.
 *   - **confirm** — the impact is known: keeps the standard delete copy and adds
 *     the agent block below it (shiki's 2026-07-07 gate), then takes one
 *     confirm. An EMPTY impact is a legitimate read (an empty computer), so it
 *     renders the same screen with no block — and still sends the same flag,
 *     because consent is over the computer, not over the rows that happened to be
 *     listed (Cindy: gating the flag on "preview was non-empty" would smuggle a
 *     1-bit staleness check back into the protocol).
 *   - **unavailable** — the read failed. Fail closed: no Delete at all, retry or
 *     cancel. "Couldn't check" must never render as "nothing on it".
 * The impact names agents only (#task:1809): shared workspaces are not rooted on
 * a computer and survive it, so there is no unowned-workspace screen to reach.
 * Execution failures add to the current screen rather than replacing it, except
 * `pending` (an agent is still materializing — a transient wait-and-retry that
 * the preview deliberately does not predict).
 */
export function DeleteComputerSection({ host }: { host: HostDto }): JSX.Element {
  const del = useDeleteComputerMutation(host.id);
  const [open, setOpen] = useState(false);
  const [failure, setFailure] = useState<DeleteComputerFailure | null>(null);
  // Fresh read per open (see `useHostResourcesQuery`): the confirm must show
  // current ownership, not a list cached from a previous visit — which is why
  // `isFetching` is part of the gate, not just `isPending`.
  const resources = useHostResourcesQuery(host.id, { enabled: open });
  const phase = resolveDeleteComputerPhase({
    isPending: resources.isPending,
    isFetching: resources.isFetching,
    isError: resources.isError,
    data: resources.data,
  });

  const openConfirm = (): void => {
    setFailure(null);
    setOpen(true);
  };

  const handleDelete = async (): Promise<void> => {
    setFailure(null);
    try {
      await del.mutateAsync({ cascadeDependents: true });
      // Success: the deleted row drops out of the host list, which clears the
      // selection app-side. Close the dialog; this view will unmount/remount.
      setOpen(false);
    } catch (err) {
      setFailure(resolveDeleteFailure(err));
    }
  };

  const label = host.display_label?.trim() || host.name;

  return (
    <>
      <Button variant="destructive-outline" className="gap-2 w-full" onClick={openConfirm}>
        <DeleteIcon className="size-3.5" />
        {computerCopy().deleteComputer}
      </Button>

      <Dialog
        open={open}
        onOpenChange={(next) => {
          // Reset transient state whenever the dialog fully closes so a
          // re-open always starts from a fresh read + clean confirm.
          setOpen(next);
          if (!next) setFailure(null);
        }}
      >
        <DialogContent className="max-w-md">
          {failure === "pending" ? (
            <>
              <DialogHeader>
                <DialogTitle>{computerCopy().deletePendingTitle}</DialogTitle>
                <DialogDescription>{computerCopy().deletePendingBody}</DialogDescription>
              </DialogHeader>
              <DialogFooter>
                <Button variant="cancel" onClick={() => setOpen(false)} disabled={del.isPending}>
                  {computerCopy().cancel}
                </Button>
                <Button
                  variant="primary"
                  className="gap-2"
                  onClick={() => void handleDelete()}
                  disabled={del.isPending}
                >
                  {del.isPending ? <SpinnerIcon className="size-3.5 animate-spin" /> : null}
                  {computerCopy().deleteRetry}
                </Button>
              </DialogFooter>
            </>
          ) : phase.kind === "unavailable" ? (
            <>
              <DialogHeader>
                <DialogTitle>{computerCopy().deleteConfirmTitle}</DialogTitle>
                <DialogDescription>{computerCopy().deleteImpactUnavailable}</DialogDescription>
              </DialogHeader>
              <DialogFooter>
                <Button variant="cancel" onClick={() => setOpen(false)}>
                  {computerCopy().cancel}
                </Button>
                <Button
                  variant="primary"
                  className="gap-2"
                  onClick={() => void resources.refetch()}
                  disabled={resources.isFetching}
                >
                  {resources.isFetching ? <SpinnerIcon className="size-3.5 animate-spin" /> : null}
                  {computerCopy().deleteRetry}
                </Button>
              </DialogFooter>
            </>
          ) : (
            <>
              <DialogHeader>
                <DialogTitle>{computerCopy().deleteConfirmTitle}</DialogTitle>
                {/* shiki interaction gate (2026-07-07): the impact does NOT
                    replace the standard delete copy — it keeps it and adds the
                    blocks below, so the user reads the same base consequence
                    plus exactly what else goes. */}
                <DialogDescription>{computerCopy().deleteConfirmBody}</DialogDescription>
              </DialogHeader>
              {phase.kind === "confirm" && phase.agents.length > 0 ? (
                <CascadeImpact agents={phase.agents} />
              ) : null}
              {failure === "generic" ? (
                <p className="text-xs text-error">{computerCopy().deleteFailed}</p>
              ) : null}
              <DialogFooter>
                <Button variant="cancel" onClick={() => setOpen(false)} disabled={del.isPending}>
                  {computerCopy().cancel}
                </Button>
                <Button
                  variant="destructive"
                  className="gap-2"
                  onClick={() => void handleDelete()}
                  // Armed only once the impact is on screen: the `loading` phase
                  // renders this same confirm, so this guard is what stops a
                  // confirm of an impact the user has not seen.
                  disabled={phase.kind !== "confirm" || del.isPending}
                  aria-label={`${computerCopy().deleteComputer}: ${label}`}
                >
                  {del.isPending || phase.kind === "loading" ? (
                    <SpinnerIcon className="size-3.5 animate-spin" />
                  ) : (
                    <DeleteIcon className="size-3.5" />
                  )}
                  {del.isPending ? computerCopy().deleting : computerCopy().deleteConfirm}
                </Button>
              </DialogFooter>
            </>
          )}
        </DialogContent>
      </Dialog>
    </>
  );
}

/**
 * #task:1380 — cascade impact block. shiki interaction gate (2026-07-07):
 * shown UNDER the standard delete confirm (never replacing it) as a pale-amber
 * callout that names the consequence ("Agents will be removed") and lists the
 * exact agents that go with the computer.
 *
 * The `--warning-surface` fill is a per-theme soft caution tint (a clean pale
 * goose-yellow on the light themes, a translucent amber veil on slack-dark) —
 * distinct from `--warning-soft`, which is the brown-gold `--warning` at low
 * alpha and muddies into a dirty tan when used as a large fill (shiki
 * 2026-07-07). Radius follows `--control-radius` (poster = 0 poster corners,
 * warm = rounded), matching the other in-dialog blocks.
 *
 * The list comes from `/resources` — DB ownership, so it stays populated and
 * accurate even when the computer is offline, which is the primary case for this
 * flow (never source it from the daemon host-status view, which is empty offline
 * — Alice #task:1380).
 * Each row reuses `AgentAvatar` (runtime tint) so the identities read in the
 * same visual vocab as "Agents on this machine" above.
 */
export function CascadeImpact({ agents }: { agents: HostAgentResource[] }): JSX.Element {
  return (
    <div className="rounded-(--control-radius) bg-(--warning-surface) px-3 py-2.5">
      <p className="text-(length:--text-meta) font-semibold uppercase tracking-wider text-warning">
        {computerCopy().deleteCascadeImpactTitle}
      </p>
      <ul className="mt-2 flex flex-col gap-1.5" aria-label={t("computers.agentsToBeRemoved")}>
        {agents.map((agent) => {
          const knownRuntime = RuntimeKindSchema.safeParse(agent.runtime);
          return (
            <li key={agent.id} className="flex items-center gap-2">
              {knownRuntime.success ? (
                <AgentAvatar
                  runtime={knownRuntime.data as RuntimeKind}
                  size={AVATAR_PX.sm}
                  alt={agent.display_name}
                />
              ) : (
                <span
                  className="inline-flex size-5 shrink-0 items-center justify-center rounded-none bg-bg-elevated text-text-muted"
                  aria-hidden
                >
                  <CpuIcon className="size-3" />
                </span>
              )}
              <span className="min-w-0 flex-1 truncate text-sm text-text-primary">
                {agent.display_name}
              </span>
            </li>
          );
        })}
      </ul>
    </div>
  );
}
