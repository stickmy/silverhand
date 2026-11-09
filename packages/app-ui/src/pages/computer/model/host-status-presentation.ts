/**
 * `#task:808` slice 4 — UI mapper for `HostStatusResponse` arms.
 *
 * Deliberately separate from the Settings → Connection pane's own
 * Desktop→Server projection (`settings/connection-readiness.ts`) because
 * the source data is different: that one works on `BackendInfoSnapshot`
 * (LOCAL host only, via the IPC backend-info snapshot) and answers "can
 * this desktop reach its server"; this file works on the wire-shape
 * `HostStatusResponse` returned by `GET /api/hosts/:id/status` for ANY
 * host (forward-compatible with future remote hosts — shiki: "功能应是
 * hosts 主语的通用功能，不是 local only").
 *
 * Why a fresh mapper rather than collapsing both into one:
 *   - The two surfaces have different sources of truth. The local pane
 *     reads a privileged daemon-attachment snapshot the renderer always
 *     has; the per-host detail reads the public HTTP arm that any host
 *     could reasonably return. Merging would force one to fake fields
 *     it doesn't have, blurring the boundary.
 *   - The `availability` enum carries a `revoked` arm with no parallel
 *     in `local_host_status` (the local pane has `revoked` as a status
 *     value, but it's never accompanied by an `unreachable_reason`).
 *   - Forward compat: when remote-host support lands, the right pane
 *     will render every host through this mapper without any
 *     local/remote branching at the component layer.
 *
 * What the split does NOT license is two vocabularies: the shared
 * availability words come from `computerCopy` on both paths (§7.1).
 *
 * Tone keys are the shared status palette (`accent` / `success` /
 * `muted` / `warning` / `error`) so the same Tailwind utilities
 * (`text-success` etc.) work in every status surface, and theme
 * tokens (studio-light.css / poster.css) need no new definitions.
 */
import type {
  HostAdmissionFailure,
  HostStatusResponse,
  HostUnreachableReason,
} from "@silverhand/shared";
import { computerCopy } from "../../../features/computers/model/computer-copy";
import { t } from "@/lib/i18n";

export type HostStatusTone = "accent" | "success" | "muted" | "warning" | "error";

export interface HostStatusView {
  /** Short product label rendered as a pill next to the host name.
   *  Speaks user terms ("Connected", "Offline", "Revoked") rather than
   *  daemon/RPC terms. */
  label: string;
  /** Optional secondary line — currently used to surface the typed
   *  `unreachable.reason` ("timed out", "couldn't reach the daemon"),
   *  the terminal `disconnected.admission_failure` cause + its one
   *  effective action, or the registration-only "(0 agents online)"
   *  string. `null` when not applicable. */
  detail: string | null;
  /** Token-key for the pill's border + text color. Shares the status
   *  palette with every other status surface so consumers reuse the same
   *  `text-{tone}` / `border-{tone}` Tailwind utilities. */
  tone: HostStatusTone;
}

/**
 * Map an `unreachable.reason` to a user-readable secondary line.
 * Daemon-language reasons stay in the wire shape; the renderer never
 * displays them verbatim.
 */
export function describeUnreachableReason(reason: HostUnreachableReason): string {
  switch (reason) {
    case "timeout":
      return t("computers.theComputerTookToo");
    case "daemon_unreachable":
      return t("computers.couldNotReachThis");
    case "protocol_error":
      return t("computers.thisComputerSentAn");
    default: {
      // Exhaustive — adding a new reason on the schema side fails the
      // typecheck here loud-and-early rather than rendering an empty
      // detail line.
      const _exhaustive: never = reason;
      return _exhaustive;
    }
  }
}

/**
 * `#task:1631` — the notice text for an offline computer whose last
 * authenticated connection attempt was **refused**, i.e. offline that
 * will not self-heal.
 *
 * Rendered as a page-level caution notice on the computer page, NOT as
 * part of the status chrome (shiki 2026-07-26: 不应该是横幅,而是
 * computer page 中某个地方给一个提示). The availability word stays
 * "Offline" — this is the *cause* of offline, not a fifth status — so
 * the two surfaces divide cleanly: the pill states the connection fact,
 * this states the one action that changes it.
 *
 * Written as one fact clause + one action clause, the same two-part
 * shape `describeProtocolSkew` (features/me — the app-wide skew banner)
 * uses, and for the same reason: the fact tells the user why, the action
 * tells them the single thing that changes it.
 *
 * Three deliberate copy rules:
 *   - **The action clause names the artifact, not "the software".** shiki
 *     2026-07-26: "Update the server" (and, by the same logic, a bare
 *     "Update Silverhand") does not tell the reader what to run. The
 *     stale side of a user-managed computer is the published package the
 *     connection command installs, so the action names it verbatim
 *     (`computerCopy.connectionPackageName`) — a copy-pasteable
 *     identifier, the same class as the connection command string, which
 *     is why the §7.1 gate exempts the literal and still rejects the bare
 *     word in prose.
 *
 *     An app-managed computer runs the copy that ships inside the
 *     Silverhand app, so naming the package THERE would be a dead end
 *     (updating a package it never installed changes nothing) — hence the
 *     `appManaged` split. Same reason both skew directions are described:
 *     an action clause pointing at the wrong artifact is worse than a
 *     vague one.
 *   - **The protocol integers appear in the fact clause only.** The
 *     app-wide banner omits them entirely (`#task:1462` — with one
 *     implied subject they are diagnostic noise). Here they carry real
 *     information: with several computers registered, "which one is
 *     stale, and against what" is exactly what the user must know to
 *     pick the machine to upgrade. The action clause stays version-free,
 *     so nobody has to translate a number into a step.
 *   - **The subject is named.** This notice sits on one computer's page
 *     while other computers may be fine, so it says which side is
 *     behind instead of the banner's blanket "this version".
 *
 * Both directions are described because both are reachable: rolling the
 * server back (or forward without updating a machine) inverts which side
 * is stale, and telling someone to upgrade the machine that is already
 * ahead sends them down a dead end.
 *
 * Jargon-free per the §7.1 copy gate: the wire-level words (`daemon`,
 * `host`) never reach prose — "Silverhand on this computer" is the
 * product-level name for the same thing.
 */
export function describeAdmissionFailure(
  failure: HostAdmissionFailure,
  options: { appManaged: boolean },
): string {
  switch (failure.kind) {
    case "protocol_mismatch": {
      const offered = failure.offered_protocol_version;
      const expected = failure.expected_protocol_version;
      if (offered > expected) {
        return t("computers.protocolTooNew", { offered, expected });
      }
      const target = options.appManaged
        ? t("computers.silverhandOnThisComputer")
        : t("computers.packageOnThisComputer", { name: computerCopy().connectionPackageName });
      return t("computers.protocolTooOld", { offered, expected, target });
    }
    default: {
      // Exhaustive — a new terminal admission reason on the schema side
      // must land its own copy here rather than degrade to the generic
      // "not connected right now" line, which promises a self-heal that
      // by definition will not happen.
      const _exhaustive: never = failure.kind;
      return _exhaustive;
    }
  }
}

/**
 * Compute the pill view for a `HostStatusResponse`. The detail view
 * still renders `data.agents[]` and the snapshot card independently —
 * this function only describes the **availability** of the host as a
 * whole.
 */
export function describeHostStatus(status: HostStatusResponse): HostStatusView {
  switch (status.availability) {
    case "available":
      return {
        label: computerCopy().availabilityConnected,
        detail: null,
        tone: "success",
      };
    case "disconnected":
      // `admission_failure` does NOT change the availability view: same
      // word, same muted tone (`#task:1631` + shiki 2026-07-26). The
      // "you must upgrade this machine" fact is a page-level notice on
      // the computer page (`AdmissionFailureNotice` in `HostDetailView`),
      // not a second voice in the status chrome — the pill answers "is
      // it connected", the notice answers "what do I do about it".
      return {
        label: computerCopy().availabilityOffline,
        detail: t("computers.thisComputerIsNot"),
        tone: "muted",
      };
    case "unreachable":
      return {
        label: computerCopy().availabilityUnreachable,
        detail: describeUnreachableReason(status.reason),
        tone: "warning",
      };
    case "revoked":
      return {
        label: computerCopy().lifecycleRevoked,
        detail: t("computers.thisComputerHasBeen"),
        tone: "muted",
      };
    default: {
      // Exhaustive over the discriminator — a new arm on the schema
      // fails the typecheck rather than silently falling through.
      const _exhaustive: never = status;
      return _exhaustive;
    }
  }
}

/** Byte count as gigabytes for a used / total reading: one decimal below
 *  100 GB, whole numbers above. `base` follows the OS convention of the
 *  quantity (binary for memory, decimal for disk). */
export function formatGigabytes(bytes: number, base: 1000 | 1024): string {
  const gb = bytes / base ** 3;
  return gb < 100 ? (Math.round(gb * 10) / 10).toString() : Math.round(gb).toString();
}

/**
 * Pure helper: format an `uptime_ms` integer as a short string for the
 * snapshot card ("12s", "3m", "2h 14m", "5d 6h"). Always relative — the
 * detail view shows wall-clock `started_at` separately if needed.
 */
export function formatUptime(uptimeMs: number): string {
  const totalSeconds = Math.max(0, Math.floor(uptimeMs / 1000));
  if (totalSeconds < 60) return `${totalSeconds}s`;
  const minutes = Math.floor(totalSeconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  const remMinutes = minutes - hours * 60;
  if (hours < 24) {
    return remMinutes > 0 ? `${hours}h ${remMinutes}m` : `${hours}h`;
  }
  const days = Math.floor(hours / 24);
  const remHours = hours - days * 24;
  return remHours > 0 ? `${days}d ${remHours}h` : `${days}d`;
}
