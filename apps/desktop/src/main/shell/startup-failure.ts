import type { BootFailureCode, BootState } from "@silverhand/client";
import { ProfileUnusableError, RemoteOnlyServerProfileRequiredError } from "./backend-boot-mode.js";

export type StartupFailureNotice = Extract<BootState, { kind: "failed" }>["notice"];
export type StartupDiagnostics = Extract<BootState, { kind: "failed" }>["diagnostics"];

const DETAIL_LIMIT = 200;

function shorten(value: string): string {
  const collapsed = value.replace(/\s+/g, " ").trim();
  if (collapsed.length <= DETAIL_LIMIT) return collapsed;
  return `${collapsed.slice(0, DETAIL_LIMIT - 1)}…`;
}

export function describeStartupFailure(error: unknown): StartupFailureNotice {
  if (error instanceof ProfileUnusableError) {
    // The error message is a repair essay for the log; the UI needs the classifier's reason.
    return {
      code: "profile_invalid",
      detail: shorten(error.classifyMessage),
      profileUrl: error.profileUrl,
    };
  }
  if (error instanceof RemoteOnlyServerProfileRequiredError) {
    return { code: "remote_profile_required", detail: shorten(error.reason), profileUrl: null };
  }
  const message = error instanceof Error ? error.message : String(error);
  return {
    code: "startup_failed",
    detail: message.trim().length > 0 ? shorten(message) : null,
    profileUrl: null,
  };
}

const ADDRESS_FAILURES: ReadonlySet<BootFailureCode> = new Set([
  "profile_invalid",
  "remote_profile_required",
]);

/** Address failures are repaired in place; there is no entry to switch to the embedded backend. */
export function failedBootState(input: {
  notice: StartupFailureNotice;
  diagnostics: StartupDiagnostics;
}): BootState {
  return {
    kind: "failed",
    notice: input.notice,
    repair: ADDRESS_FAILURES.has(input.notice.code) ? "server_address" : "none",
    diagnostics: input.diagnostics,
  };
}

export function formatDiagnostics(state: Extract<BootState, { kind: "failed" }>): string {
  const { notice, diagnostics } = state;
  const commit = diagnostics.dirty ? `${diagnostics.commit} (modified)` : diagnostics.commit;
  const lines = [
    `Code: ${notice.code}`,
    notice.profileUrl ? `Server: ${notice.profileUrl}` : null,
    notice.detail ? `Detail: ${notice.detail}` : null,
    `Version: ${diagnostics.version ?? "unknown"} · ${commit}`,
    `Platform: ${process.platform} ${process.arch}`,
    diagnostics.logsDir ? `Logs: ${diagnostics.logsDir}` : null,
  ];
  return lines.filter((line): line is string => line !== null).join("\n");
}

export const ISSUE_URL = "https://github.com/stickmy/silverhand/issues/new";

export function issueUrl(state: Extract<BootState, { kind: "failed" }>): string {
  const url = new URL(ISSUE_URL);
  url.searchParams.set("title", `[Startup] ${state.notice.code}`);
  url.searchParams.set("body", `## Diagnostics\n\n\`\`\`\n${formatDiagnostics(state)}\n\`\`\`\n`);
  return url.toString();
}

export const STARTUP_FAILURE_TITLE = "Silverhand could not start";
