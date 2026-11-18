import { classifyServerUrl, type ServerConnectionProfileView } from "@silverhand/shared";

export type BackendBootMode =
  | {
      mode: "embedded";
      reason: "no_profile" | "forced_by_env";
      /** The overridden profile URL under `forced_by_env` (for log lines); otherwise empty. */
      profileUrl: string;
    }
  | {
      mode: "remote";
      /**
       * The classifier-validated remote URL. Bootstrap consumers
       * should use this string as the source of truth — do NOT
       * re-read the profile bytes separately to avoid TOCTOU.
       */
      serverUrl: string;
      /**
       * `true` when the URL was admitted only because this client
       * explicitly allows non-loopback http (via dev env or packaged
       * build option). Bootstrap consumers should emit a visible WARN
       * line referencing the runbook on this branch.
       */
      insecure: boolean;
    }
  | {
      /**
       * A profile is present but the boot-time classifier refused
       * it. Bootstrap throws {@link ProfileUnusableError}; never
       * silently falls through to embedded (Cindy D4.a Commit 3
       * review P1-B). Repair path: user clears the profile in
       * Settings, or sets `SILVERHAND_FORCE_EMBEDDED=1` to override.
       */
      mode: "profile_invalid";
      /** Raw URL string from the profile (for log lines + UI). */
      profileUrl: string;
      /**
       * The classifier's raw message so the error / log carries
       * enough context to debug without round-tripping through the
       * file. Typically points at the specific gate that fired
       * ("non-loopback http requires SILVERHAND_ALLOW_INSECURE_REMOTE=1",
       * "disguised loopback form", etc.).
       */
      classifyMessage: string;
    };

/**
 * Inputs to {@link decideBackendBootMode}. Kept as a plain options
 * object so callers can read the env / profile once and pass the
 * snapshot in (no hidden global reads inside the decision function).
 */
export interface DecideBackendBootModeInput {
  /**
   * The redacted profile view from
   * {@link readServerConnectionProfileViewOrNull}, or `null` when
   * the profile file is absent (first run) or unreadable. We accept
   * the **view** rather than the full profile because (a) the
   * decision only needs `server_url`, and (b) the redacted view is
   * what bootstrap already has on hand (the full profile
   * is read only when the writer needs to bump version).
   */
  profileView: ServerConnectionProfileView | null;
  /**
   * Value of `readForceEmbeddedFlag(process.env)`. When `true` the
   * decision short-circuits to `embedded/forced_by_env` regardless
   * of profile state — see {@link SILVERHAND_FORCE_EMBEDDED_ENV}.
   */
  forceEmbedded: boolean;
  /**
   * Value of `readAllowInsecureRemoteFlag(process.env)`. Passed
   * through to {@link classifyServerUrl} so a loopback-shaped URL
   * always admits (the flag does not affect loopback), an https
   * remote URL admits, and an http remote URL admits only when the
   * flag is on. Without this we'd classify-fail any http remote
   * profile on every boot even when the user explicitly committed
   * it under the dev flag.
   */
  allowInsecureRemote: boolean;
}

export function decideBackendBootMode(input: DecideBackendBootModeInput): BackendBootMode {
  if (input.forceEmbedded) {
    return {
      mode: "embedded",
      reason: "forced_by_env",
      profileUrl: input.profileView?.server_url ?? "",
    };
  }
  if (input.profileView === null) {
    return {
      mode: "embedded",
      reason: "no_profile",
      profileUrl: "",
    };
  }
  const classification = classifyServerUrl(input.profileView.server_url, {
    allowInsecureRemote: input.allowInsecureRemote,
  });
  if (!classification.ok) {
    return {
      mode: "profile_invalid",
      profileUrl: input.profileView.server_url,
      classifyMessage: classification.message,
    };
  }
  // A saved profile always means remote, loopback included (e.g. an SSH tunnel):
  // the embedded backend binds its own port and never reads the profile.
  return {
    mode: "remote",
    serverUrl: classification.url.origin,
    insecure: classification.kind === "remote" && classification.insecure,
  };
}

export class ProfileUnusableError extends Error {
  readonly profileUrl: string;
  readonly classifyMessage: string;
  constructor(decision: Extract<BackendBootMode, { mode: "profile_invalid" }>) {
    super(
      `A server connection profile is present but the boot-time URL ` +
        `classifier refused it. ` +
        `Profile URL: '${decision.profileUrl}'. ` +
        `Classifier message: ${decision.classifyMessage} ` +
        `Repair options: ` +
        `(1) restore the caller-side opt-in that made the URL acceptable ` +
        `at write time (for example the packaged insecure-remote build ` +
        `option, or SILVERHAND_ALLOW_INSECURE_REMOTE=1 in dev) and relaunch; ` +
        `(2) set SILVERHAND_FORCE_EMBEDDED=1 to boot the embedded backend ` +
        `(this ignores the profile but does NOT delete it — once the UI ` +
        `is up you can clear the profile via Settings → "Connect to ` +
        `backend" → Clear saved profile, then unset FORCE_EMBEDDED).`,
    );
    this.name = "ProfileUnusableError";
    this.profileUrl = decision.profileUrl;
    this.classifyMessage = decision.classifyMessage;
  }
}

export class RemoteOnlyServerProfileRequiredError extends Error {
  readonly reason: string;
  constructor(reason: string) {
    super(
      `This Silverhand Desktop package was built as remote-only, but no ` +
        `usable remote server profile is available. ${reason} ` +
        `Repackage with --server-url <https-origin>, or save a remote server ` +
        `profile before launching this build.`,
    );
    this.name = "RemoteOnlyServerProfileRequiredError";
    this.reason = reason;
  }
}

export type BackendStartupPlan =
  | {
      mode: "embedded";
      /**
       * Mirrors `BackendBootMode.embedded.reason` — preserved so the
       * bootstrap site's log line and any future "embedded but for
       * what reason" UI keeps the same discriminator.
       */
      embeddedReason: "no_profile" | "forced_by_env";
    }
  | {
      mode: "remote";
      /** Canonical origin URL (post-classify + origin-normalize). */
      serverUrl: string;
      /** Admitted via explicit insecure-remote opt-in. */
      insecure: boolean;
    }
  | {
      /**
       * `profile_invalid` decision arm — the bootstrap site MUST
       * throw {@link ProfileUnusableError} on this plan. The
       * adapter does not throw; bootstrap consumes the plan and
       * decides what to do, so all "if I see this plan I do X"
       * branches are visible in one place.
       */
      mode: "refuse";
      reason: "profile_invalid";
      profileUrl: string;
      classifyMessage: string;
    };

export function planBackendStartup(decision: BackendBootMode): BackendStartupPlan {
  switch (decision.mode) {
    case "embedded":
      return { mode: "embedded", embeddedReason: decision.reason };
    case "remote":
      return {
        mode: "remote",
        serverUrl: decision.serverUrl,
        insecure: decision.insecure,
      };
    case "profile_invalid":
      return {
        mode: "refuse",
        reason: "profile_invalid",
        profileUrl: decision.profileUrl,
        classifyMessage: decision.classifyMessage,
      };
    default: {
      const exhaustive: never = decision;
      throw new Error(`planBackendStartup: unhandled boot decision: ${JSON.stringify(exhaustive)}`);
    }
  }
}

export interface ExecuteBackendStartupDeps<RunningBackendT, RemoteBackendHandleT> {
  /**
   * Reads `~/.silverhand/server-config.json`, creates a fresh embedded
   * default if missing/corrupt. Embedded-only — calling on a remote
   * profile would touch the legacy artifact (the bug 5d-fixup is
   * preventing).
   */
  loadOrCreateServerConfig: () => Promise<unknown>;
  /**
   * Starts the embedded backend with the given config and persists
   * the resolved `serverConfig` (with the chosen port) back to disk.
   * The two side effects are bundled here because they are always
   * paired in bootstrap and have the same embedded-only scope.
   */
  startEmbeddedAndPersistConfig: (config: unknown) => Promise<RunningBackendT>;
  /**
   * Builds the remote-arm handle from the plan's serverUrl + insecure
   * flag. Trivial in production (`buildRemoteBackendHandle`); a fake
   * in tests.
   */
  buildRemoteHandle: (input: { serverUrl: string; insecure: boolean }) => RemoteBackendHandleT;
  /**
   * Refuse arm — defense-in-depth seam. In production `bootstrap()`
   * intercepts `startupPlan.mode === 'refuse'` BEFORE invoking the
   * adapter and throws `ProfileUnusableError` itself, so the
   * executor's refuse branch is unreachable under current call
   * ordering. Kept here so a future refactor that drops the
   * pre-daemon guard still fails loud (the adapter calls
   * `deps.refuse(plan)` which is typed `(plan) => never`). Tests
   * cover both the called-and-throws and called-and-buggy-returns
   * cases for that scenario.
   */
  refuse: (plan: Extract<BackendStartupPlan, { mode: "refuse" }>) => never;
}

export type ExecutedBackendHandle<RunningBackendT, RemoteBackendHandleT> =
  | { kind: "embedded"; running: RunningBackendT }
  | RemoteBackendHandleT;

export async function executeBackendStartup<RunningBackendT, RemoteBackendHandleT>(
  plan: BackendStartupPlan,
  deps: ExecuteBackendStartupDeps<RunningBackendT, RemoteBackendHandleT>,
): Promise<ExecutedBackendHandle<RunningBackendT, RemoteBackendHandleT>> {
  switch (plan.mode) {
    case "refuse":
      // Defense-in-depth path. Production `bootstrap()` intercepts
      // `mode === 'refuse'` BEFORE calling the adapter (so a profile
      // classifier reject fails loud before any daemon spawn / IPC
      // subscribe / WS init), which means this arm is unreachable
      // under current call ordering. We keep it here so a future
      // refactor that drops the pre-daemon guard still fails loud:
      // `deps.refuse(plan)` is typed `(plan) => never` and throws
      // `ProfileUnusableError`. The trailing throw catches a buggy
      // refuse impl that returns instead of throwing (regression
      // bait).
      deps.refuse(plan);
      throw new Error("executeBackendStartup: refuse() returned (expected `never`)");
    case "embedded": {
      const config = await deps.loadOrCreateServerConfig();
      const running = await deps.startEmbeddedAndPersistConfig(config);
      return { kind: "embedded", running };
    }
    case "remote":
      return deps.buildRemoteHandle({
        serverUrl: plan.serverUrl,
        insecure: plan.insecure,
      });
    default: {
      const exhaustive: never = plan;
      throw new Error(`executeBackendStartup: unhandled plan: ${JSON.stringify(exhaustive)}`);
    }
  }
}
