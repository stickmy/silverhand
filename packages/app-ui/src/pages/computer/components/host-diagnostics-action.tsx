import { useEffect, useState, type JSX } from "react";
import { CheckIcon, CopyIcon } from "@/icons";
import {
  buildHostDiagnosticsText,
  type HostDiagnosticsFetchErrorKind,
  type HostDiagnosticsFetchStatus,
  type HostDiagnosticsInput,
  type HostDto,
  type HostStatusResponse,
} from "@silverhand/shared";
import { Button } from "@/components/ui/button";
import { ApiError } from "@/lib/api";
import { computerCopy } from "../../../features/computers/model/computer-copy";
import {
  useBackendInfoSnapshot,
  useHostStatusQuery,
  type BackendInfoSnapshot,
} from "../../../features/computers/hooks/hooks";

/**
 * #task:1445 — the manual "Copy diagnostics" affordance for a computer.
 *
 * v1 is copy-only: no upload, no report-issue. The clipboard payload is the
 * shared `buildHostDiagnosticsText` output — a redacted, canonical, plain-text
 * report the user pastes into a bug report / chat. Format + redaction live in
 * the shared builder; this component is source-blind (it assembles the narrow
 * `HostDiagnosticsInput` and copies verbatim, never formats the body itself).
 *
 * **Redaction is a compile-time boundary** (Alice/Cindy #task:1445): the input
 * is assembled by EXPLICIT per-field assignment from already-fetched safe DTOs
 * — never `spread`/`as` of a raw `HostDto` / status DTO — so a secret-shaped
 * field can't ride in. `HostDiagnosticsInput` has no token/proxy/log/config/raw-
 * error field to assign in the first place; the builder's own per-field copy is
 * the runtime backstop under this.
 *
 * **Always available, regardless of live status.** Diagnostics are most useful
 * exactly when the computer is offline/unreachable or its status failed to
 * load, so this renders OUTSIDE `HostDetailView`'s status-conditional branches
 * (like Delete). Missing data degrades to `(unavailable)` in the builder, so the
 * button is never disabled and needs no separate error state — only a transient
 * copied confirmation. It reads its own (React-Query-deduped) status + backend
 * snapshot so the caller only passes `host`.
 *
 * Renders a BARE button, not its own framed section (#task:1619): it is the
 * first row of the page's footer action group, which owns the divider and the
 * spacing.
 */
export function HostDiagnosticsAction({ host }: { host: HostDto }): JSX.Element {
  // Same query keys HostDetailView already mounts — React Query returns the
  // cached data (staleTime: Infinity), so this adds no fetch.
  const statusQuery = useHostStatusQuery(host.id);
  const backendInfo = useBackendInfoSnapshot();
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    if (!copied) return;
    const t = window.setTimeout(() => setCopied(false), 1800);
    return () => window.clearTimeout(t);
  }, [copied]);

  const handleCopy = async (): Promise<void> => {
    const input: HostDiagnosticsInput = {
      generated_at: Date.now(),
      computer: toComputerInput(host),
      status: statusQuery.data ? toStatusInput(statusQuery.data) : null,
      app: toAppInput(backendInfo.data),
      fetch: {
        // The computer DTO is a prop on this page — always resolved here.
        computer: "success",
        status: toFetchStatus(statusQuery),
        error_kind: toErrorKind(statusQuery.error),
      },
    };
    await navigator.clipboard.writeText(buildHostDiagnosticsText(input));
    setCopied(true);
  };

  return (
    <Button variant="neutral" className="gap-2 w-full" onClick={() => void handleCopy()}>
      {copied ? <CheckIcon className="size-3.5" /> : <CopyIcon className="size-3.5" />}
      {copied ? computerCopy().copied : computerCopy().copyDiagnostics}
    </Button>
  );
}

/* ─── source → narrow allowlist mapping (explicit per field, no spread) ─── */

// Exported for unit tests: these are the renderer-side half of the redaction
// boundary — explicit per-field picks that drop anything not in the narrow
// input type, even secret-shaped runtime extras on the source DTOs.
export function toComputerInput(host: HostDto): HostDiagnosticsInput["computer"] {
  return {
    id: host.id,
    name: host.name,
    display_label: host.display_label,
    kind: host.kind,
    provisioning_source: host.provisioning_source,
    status: host.status,
    placeable: host.placeable,
    created_at: host.created_at,
    last_seen_at: host.last_seen_at,
    activated_at: host.activated_at,
    finalized_at: host.finalized_at,
    revoked_at: host.revoked_at,
    connected_host_name: host.connected_host_name,
  };
}

type DiagnosticsStatusInput = NonNullable<HostDiagnosticsInput["status"]>;

export function toStatusInput(data: HostStatusResponse): DiagnosticsStatusInput {
  // Agents: registration view, safe subset only (drop agent_id / display_name
  // / avatar_url; keep handle / runtime / online).
  const agents = data.agents.map((agent) => ({
    handle: agent.handle,
    runtime: agent.runtime,
    online: agent.online,
  }));
  if (data.availability === "available") {
    return {
      availability: data.availability,
      last_seen_at: data.last_seen_at ?? null,
      agents,
      snapshot: {
        os: data.snapshot.os,
        os_arch: data.snapshot.os_arch,
        daemon_version: data.snapshot.daemon_version,
        daemon_pid: data.snapshot.daemon_pid,
        started_at: data.snapshot.started_at,
        uptime_ms: data.snapshot.uptime_ms,
      },
    };
  }
  if (data.availability === "unreachable") {
    return {
      availability: data.availability,
      reason: data.reason,
      last_seen_at: data.last_seen_at ?? null,
      agents,
    };
  }
  // disconnected | revoked — no snapshot, no reason.
  return {
    availability: data.availability,
    last_seen_at: data.last_seen_at ?? null,
    agents,
  };
}

function toAppInput(info: BackendInfoSnapshot | undefined): HostDiagnosticsInput["app"] {
  if (!info) return null;
  return {
    server_url: info.url,
    mode: info.mode,
  };
}

function toFetchStatus(query: {
  isLoading: boolean;
  isError: boolean;
}): HostDiagnosticsFetchStatus {
  if (query.isError) return "error";
  if (query.isLoading) return "loading";
  return "success";
}

/**
 * Normalize a client query error to the allowlisted category — never the raw
 * message (which can carry a request URL / query token). `HostDiagnosticsInput`
 * has no field for a raw message, so this is the only error shape that can be
 * assigned.
 */
export function toErrorKind(error: unknown): HostDiagnosticsFetchErrorKind | undefined {
  if (!error) return undefined;
  if (error instanceof ApiError) {
    if (error.kind === "backend_unreachable" || error.status === 0) {
      return "network";
    }
    if (error.status === 403) return "forbidden";
    if (error.status === 404) return "not_found";
    return "unknown";
  }
  return "unknown";
}
