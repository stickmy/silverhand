/**
 * CLI error contract — the single source of truth for what stderr looks
 * like, what exit codes mean, and how HTTP failures from the backend
 * map onto stable, agent-readable error codes.
 *
 * Scope:
 * - `code`: a short, stable identifier from `ERROR_CODES`. Backend
 *   `internal/*` routes either return one of these directly in
 *   `{error: {code, message}}` shape, or the CLI infers it from the
 *   HTTP status. Either way, the agent gets the same code.
 * - `retriable`: whether the agent can retry without changing input.
 *   Validation/auth/not-found are NOT retriable; rate-limit and
 *   upstream-unavailable are.
 * - exit codes:
 *     0 = ok
 *     1 = user/input error (do not retry)
 *     2 = transient (may retry with backoff)
 *     3 = auth (stop, report to caller)
 * - stderr line:
 *     One short, friendly, action-oriented line. Never a stack trace,
 *     never a raw HTTP status string.
 *     In `--json` mode (default), the stderr line is a one-line JSON
 *     `{error: {code, message, retriable}}` object so the agent can
 *     parse and branch.
 *
 * The contract is exercised by `tests/errors.test.ts` and is the
 * agreement Cindy's backend slice (#193) maps backend errors onto.
 */

export const ERROR_CODES = [
  'not_found',
  'forbidden',
  'unauthorized',
  'validation_error',
  'conflict',
  'rate_limited',
  'upstream_unavailable',
  'internal',
] as const;

export type ErrorCode = (typeof ERROR_CODES)[number];

/**
 * Optional, descriptive sub-classification of a `validation_error`.
 * `code` remains the stable behavioral contract (drives `exitCodeFor`
 * and `retriable`); `subkind` lets agents and telemetry distinguish
 * input-shape variants without enlarging the closed `code` enum.
 *
 * The CLI-side union below is the source of truth. The shared
 * `CliTraceInputSchema.error_subkind` field is a wide
 * `string().max(64)` so adding a new subkind here does not require a
 * shared schema bump.
 */
export type ErrorSubkind =
  | 'flag_removed'
  | 'verb_removed'
  | 'missing_required_flag'
  | 'unexpected_arguments'
  | 'stdin_is_tty'
  | 'empty_stdin'
  | 'invalid_utf8'
  | 'unknown_flag'
  | 'enum_invalid'
  | 'mutually_exclusive';

export interface CliErrorPayload {
  code: ErrorCode;
  /** Optional descriptive sub-classification of `code`. */
  subkind?: ErrorSubkind;
  message: string;
  retriable: boolean;
  /** Optional structured context for the agent reading stderr. NEVER
   *  forwarded to `cli-trace` — see `cli-trace.ts` for the privacy
   *  contract. Typical contents: `{flag, replacement}` for
   *  `flag_removed`, `{positional, rest}` for `unexpected_arguments`. */
  details?: Record<string, unknown>;
}

export class CliError extends Error {
  public readonly code: ErrorCode;
  public readonly subkind?: ErrorSubkind;
  public readonly retriable: boolean;
  public readonly details?: Record<string, unknown>;

  constructor(payload: CliErrorPayload) {
    super(payload.message);
    this.code = payload.code;
    this.subkind = payload.subkind;
    this.retriable = payload.retriable;
    this.details = payload.details;
  }

  toPayload(): CliErrorPayload {
    return {
      code: this.code,
      ...(this.subkind ? { subkind: this.subkind } : {}),
      message: this.message,
      retriable: this.retriable,
      ...(this.details ? { details: this.details } : {}),
    };
  }
}

/** Map an HTTP status + optional `{error:{code,message}}` body from the
 *  backend onto a `CliError` with a stable code. The CLI's job is to
 *  hide raw HTTP from the agent; if the backend already supplies a
 *  stable code we use it, otherwise we infer from status.
 *
 *  The `bodyText` parameter is the raw response body (already read
 *  upstream). We try JSON-parse defensively — backends sometimes return
 *  HTML for proxy errors. */
export function mapHttpError(status: number, bodyText: string): CliError {
  const parsed = safeParseErrorBody(bodyText);

  // Trust an explicit backend code only when it's in our enum.
  if (parsed?.code && (ERROR_CODES as readonly string[]).includes(parsed.code)) {
    const code = parsed.code as ErrorCode;
    return new CliError({
      code,
      message: parsed.message ?? defaultMessageForCode(code),
      retriable: isRetriableCode(code),
      ...(parsed.details ? { details: parsed.details } : {}),
    });
  }

  const code = inferCodeFromStatus(status);
  return new CliError({
    code,
    message: parsed?.message ?? defaultMessageForStatus(status, code),
    retriable: isRetriableCode(code),
    ...(parsed?.details ? { details: parsed.details } : {}),
  });
}

/** Wrap a low-level fetch / network failure (TCP refused, DNS fail,
 *  abort) as a transient error so the agent can retry. */
export function fromNetworkError(err: unknown): CliError {
  const reason = err instanceof Error ? err.message : String(err);
  return new CliError({
    code: 'upstream_unavailable',
    message: `cannot reach silverhand backend: ${reason}`,
    retriable: true,
  });
}

/** Map an unknown thrown value (zod parse failure, programmer error,
 *  etc.) to a `CliError`. Programmer errors land as `internal` so we
 *  don't leak stack traces; zod errors land as `validation_error`. */
export function fromUnknown(err: unknown): CliError {
  if (err instanceof CliError) return err;
  if (isZodError(err)) {
    return new CliError({
      code: 'validation_error',
      message: formatZodError(err),
      retriable: false,
    });
  }
  const reason = err instanceof Error ? err.message : String(err);
  return new CliError({
    code: 'internal',
    message: `internal cli error: ${reason}`,
    retriable: false,
  });
}

export function exitCodeFor(code: ErrorCode): 1 | 2 | 3 {
  switch (code) {
    case 'unauthorized':
    case 'forbidden':
      return 3;
    case 'rate_limited':
    case 'upstream_unavailable':
      return 2;
    default:
      return 1;
  }
}

/** Write a `CliError` to stderr per the contract.
 *  - `human` mode: short tag + message line; if `details.blocking_tasks`
 *    is present, each task gets its own follow-up line so the agent /
 *    operator can see exactly which tasks blocked the operation.
 *  - `json` mode (default): one-line JSON `{error:{code,subkind?,message,retriable,details?}}`.
 *  Never logs stack traces. The exit code is the caller's responsibility
 *  (use `exitCodeFor`). */
export function writeError(
  err: CliError,
  opts: { json: boolean; stderr?: { write(s: string): unknown } } = { json: true },
): void {
  const out = opts.stderr ?? process.stderr;
  if (opts.json) {
    out.write(JSON.stringify({ error: err.toPayload() }) + '\n');
    return;
  }
  const tag = err.subkind ? `${err.code}/${err.subkind}` : err.code;
  out.write(`silverhand: ${tag}: ${err.message}\n`);
  const blocking = readBlockingTasks(err.details);
  if (blocking && blocking.length > 0) {
    for (const t of blocking) {
      const assignee = t.assignee_name ? ` @${t.assignee_name}` : '';
      const title = t.title ? ` — ${t.title}` : '';
      out.write(`  #task:${t.task_number} (${t.status})${assignee}${title}\n`);
    }
  }
}

/** Best-effort extractor for the structured `blocking_tasks` array the
 *  backend attaches to `has_open_tasks` conflicts. Returns null when
 *  the shape doesn't match — `writeError` falls back to the message
 *  line only. We intentionally don't import the shared zod schema here:
 *  the CLI's render path must stay tolerant of slightly drifted
 *  details payloads. */
interface BlockingTaskRow {
  task_number: number;
  status: string;
  title: string;
  assignee_name: string | null;
}

function readBlockingTasks(
  details: Record<string, unknown> | undefined,
): BlockingTaskRow[] | null {
  if (!details) return null;
  const raw = details['blocking_tasks'];
  if (!Array.isArray(raw)) return null;
  const out: BlockingTaskRow[] = [];
  for (const entry of raw) {
    if (!entry || typeof entry !== 'object') continue;
    const e = entry as Record<string, unknown>;
    const num = typeof e['task_number'] === 'number' ? e['task_number'] : null;
    const status = typeof e['status'] === 'string' ? e['status'] : null;
    if (num === null || status === null) continue;
    out.push({
      task_number: num,
      status,
      title: typeof e['title'] === 'string' ? e['title'] : '',
      assignee_name:
        typeof e['assignee_name'] === 'string' ? e['assignee_name'] : null,
    });
  }
  return out;
}

// ── internals ────────────────────────────────────────────────────

function inferCodeFromStatus(status: number): ErrorCode {
  if (status === 400 || status === 422) return 'validation_error';
  if (status === 401) return 'unauthorized';
  if (status === 403) return 'forbidden';
  if (status === 404) return 'not_found';
  if (status === 409) return 'conflict';
  if (status === 429) return 'rate_limited';
  if (status >= 500 && status < 600) return 'upstream_unavailable';
  // 4xx without a more specific mapping — treat as validation so the
  // agent doesn't blindly retry.
  if (status >= 400 && status < 500) return 'validation_error';
  return 'internal';
}

function isRetriableCode(code: ErrorCode): boolean {
  return code === 'rate_limited' || code === 'upstream_unavailable';
}

function defaultMessageForCode(code: ErrorCode): string {
  switch (code) {
    case 'not_found':
      return 'resource not found';
    case 'forbidden':
      return 'forbidden';
    case 'unauthorized':
      return 'auth token rejected by backend';
    case 'validation_error':
      return 'request rejected: invalid input';
    case 'conflict':
      return 'request conflicts with current state';
    case 'rate_limited':
      return 'rate limited; back off and retry';
    case 'upstream_unavailable':
      return 'silverhand backend is unreachable';
    case 'internal':
      return 'silverhand backend reported an internal error';
  }
}

function defaultMessageForStatus(status: number, code: ErrorCode): string {
  return `${defaultMessageForCode(code)} (HTTP ${status})`;
}

interface ParsedErrorBody {
  code?: string;
  message?: string;
  details?: Record<string, unknown>;
}

function safeParseErrorBody(bodyText: string): ParsedErrorBody | null {
  if (!bodyText) return null;
  try {
    const parsed = JSON.parse(bodyText) as unknown;
    if (parsed && typeof parsed === 'object') {
      const obj = parsed as Record<string, unknown>;
      // Accept either `{error: {code, message, details?}}` or a flat
      // `{code, message, details?}`.
      const inner =
        obj.error && typeof obj.error === 'object'
          ? (obj.error as Record<string, unknown>)
          : obj;
      const code = typeof inner.code === 'string' ? inner.code : undefined;
      const message =
        typeof inner.message === 'string' ? inner.message : undefined;
      const details =
        inner.details && typeof inner.details === 'object'
          ? (inner.details as Record<string, unknown>)
          : undefined;
      if (code || message || details) return { code, message, details };
    }
  } catch {
    // not JSON; fall through and let the caller use default messages.
  }
  return null;
}

function isZodError(err: unknown): err is { errors: Array<{ path: Array<string | number>; message: string }> } {
  return (
    !!err &&
    typeof err === 'object' &&
    'name' in err &&
    (err as { name: unknown }).name === 'ZodError' &&
    Array.isArray((err as { errors?: unknown }).errors)
  );
}

function formatZodError(err: { errors: Array<{ path: Array<string | number>; message: string }> }): string {
  const first = err.errors[0];
  if (!first) return 'request rejected: invalid input';
  const path = first.path.length > 0 ? first.path.join('.') : '<root>';
  return `invalid argument \`${path}\`: ${first.message}`;
}
