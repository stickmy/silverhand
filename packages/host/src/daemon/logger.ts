/**
 * Tiny JSONL logger for the daemon process. Mirrors the format of
 * `packages/backend/src/logger.ts` so a future split can promote
 * either logger to a shared package without breaking the on-disk
 * format the user already greps with `jq`.
 *
 * Three observable differences from the backend logger:
 *
 *  1. Filename prefix is `daemon`, with UTC daily rotation and 14-file retention.
 *  2. The daemon process may run headless (no Electron). When
 *     `alsoStderr` is true, the exact JSONL line also mirrors to stderr
 *     so terminal output and disk logs share one format. We use stderr
 *     (not stdout) because stdout is reserved for the `ready` handshake
 *     line.
 *  3. No `child(bindings)`. The daemon doesn't have many concerns to
 *     subdivide; if we need scoping later, lift the backend logger
 *     into shared and use it instead.
 */
import { createDailyLogWriter } from '@silverhand/shared/node-daily-log-writer';
import {
  SILVERHAND_LOG_LEVELS,
  parseSilverhandLogLevel,
  sanitizeLogFields,
  SILVERHAND_LOG_LEVEL_ENV,
  type SilverhandLogLevel,
} from '@silverhand/shared';

type LogEventLevel = SilverhandLogLevel | 'error';

export interface DaemonLogger {
  debug(obj: unknown, msg?: string): void;
  info(obj: unknown, msg?: string): void;
  warn(obj: unknown, msg?: string): void;
  error(obj: unknown, msg?: string): void;
  /** Absolute path of the log file currently being written. */
  filePath: string;
  /** Mutable level/window state used by this daemon logger. */
  logControl: DaemonLogControl;
  /** Flush + close the underlying stream. Idempotent. */
  close(): Promise<void>;
}

export interface ConversationDebugStatus {
  enabled: boolean;
  expiresAt: number | null;
}

export interface DaemonLogControl {
  getGlobalLevel(): SilverhandLogLevel;
  setGlobalLevel(level: SilverhandLogLevel): void;
  enableConversationDebug(conversationId: string, expiresAt: number): void;
  disableConversationDebug(conversationId: string): void;
  getConversationDebugStatus(conversationId: string): ConversationDebugStatus;
}

export interface DaemonLoggerOptions {
  dir: string;
  alsoStderr?: boolean;
  base?: Record<string, unknown>;
  logControl?: DaemonLogControl;
}

export function createDaemonLogger(opts: DaemonLoggerOptions): DaemonLogger {
  const writer = createDailyLogWriter(opts.dir, 'daemon', ['daemon', 'agent-activity']);
  const alsoStderr = opts.alsoStderr ?? false;
  const base: Record<string, unknown> = { pid: process.pid, ...(opts.base ?? {}) };
  const logControl = opts.logControl ?? createDaemonLogControl(defaultDaemonLogLevel());
  const safeBase = sanitizeLogFields(base);

  const write = (level: LogEventLevel, obj: unknown, msg?: string) => {
    const rawFields =
      obj && typeof obj === 'object' && !(obj instanceof Error)
        ? (obj as Record<string, unknown>)
        : { data: serializeUnknown(obj) };
    const fields = sanitizeLogFields(rawFields);
    const now = new Date();
    const line = {
      ts: formatLocalTimestamp(now),
      ts_unix_ms: now.getTime(),
      level,
      msg: msg ?? '',
      ...safeBase,
      ...fields,
    };
    if (!shouldWrite(level, line, logControl)) return;
    const encoded = safeStringify(line);
    // A late log (e.g. a command landing during teardown) must drop,
    // not crash with ERR_STREAM_WRITE_AFTER_END.
    writer.write(encoded + '\n', now.getTime());
    if (alsoStderr) {
      process.stderr.write(encoded + '\n');
    }
  };

  return {
    get filePath() {
      return writer.filePath;
    },
    logControl,
    debug: (obj, msg) => write('debug', obj, msg),
    info: (obj, msg) => write('info', obj, msg),
    warn: (obj, msg) => write('warn', obj, msg),
    error: (obj, msg) => write('error', obj, msg),
    close: () => writer.close(),
  };
}

function formatLocalTimestamp(date: Date): string {
  const tzOffsetMinutes = -date.getTimezoneOffset();
  const sign = tzOffsetMinutes >= 0 ? '+' : '-';
  const absOffset = Math.abs(tzOffsetMinutes);
  return (
    `${date.getFullYear()}-${pad2(date.getMonth() + 1)}-${pad2(date.getDate())} ` +
    `${pad2(date.getHours())}:${pad2(date.getMinutes())}:${pad2(date.getSeconds())}.` +
    `${pad3(date.getMilliseconds())} ${sign}${pad2(Math.floor(absOffset / 60))}:` +
    `${pad2(absOffset % 60)}`
  );
}

function pad2(value: number): string {
  return String(value).padStart(2, '0');
}

function pad3(value: number): string {
  return String(value).padStart(3, '0');
}

function defaultDaemonLogLevel(): SilverhandLogLevel {
  const raw = process.env[SILVERHAND_LOG_LEVEL_ENV];
  if (raw === undefined) return 'info';
  return SILVERHAND_LOG_LEVELS.includes(raw as SilverhandLogLevel)
    ? (raw as SilverhandLogLevel)
    : parseSilverhandLogLevel(raw);
}

export function createDaemonLogControl(
  initialLevel: SilverhandLogLevel = parseSilverhandLogLevel(process.env[SILVERHAND_LOG_LEVEL_ENV]),
): DaemonLogControl {
  let globalLevel = initialLevel;
  const windows = new Map<string, { expiresAt: number; timer: ReturnType<typeof setTimeout> }>();

  const clearWindow = (conversationId: string): void => {
    const existing = windows.get(conversationId);
    if (existing) clearTimeout(existing.timer);
    windows.delete(conversationId);
  };

  const purgeExpired = (conversationId: string, now = Date.now()): void => {
    const existing = windows.get(conversationId);
    if (!existing) return;
    if (existing.expiresAt <= now) clearWindow(conversationId);
  };

  return {
    getGlobalLevel: () => globalLevel,
    setGlobalLevel: (level) => {
      globalLevel = level;
    },
    enableConversationDebug: (conversationId, expiresAt) => {
      clearWindow(conversationId);
      const delay = expiresAt - Date.now();
      if (delay <= 0) return;
      const timer = setTimeout(() => {
        windows.delete(conversationId);
      }, delay);
      timer.unref?.();
      windows.set(conversationId, { expiresAt, timer });
    },
    disableConversationDebug: (conversationId) => {
      clearWindow(conversationId);
    },
    getConversationDebugStatus: (conversationId) => {
      purgeExpired(conversationId);
      const existing = windows.get(conversationId);
      return existing
        ? { enabled: true, expiresAt: existing.expiresAt }
        : { enabled: false, expiresAt: null };
    },
  };
}

function shouldWrite(
  eventLevel: LogEventLevel,
  line: Record<string, unknown>,
  logControl: DaemonLogControl,
): boolean {
  if (eventLevel === 'error') return true;
  const conversationId = typeof line.conversationId === 'string' ? line.conversationId : null;
  const effectiveLevel =
    conversationId && logControl.getConversationDebugStatus(conversationId).enabled
      ? 'debug'
      : logControl.getGlobalLevel();
  return LEVEL_RANK[eventLevel] >= LEVEL_RANK[effectiveLevel];
}

const LEVEL_RANK: Record<LogEventLevel, number> = {
  debug: 10,
  info: 20,
  warn: 30,
  error: 40,
};

function serializeUnknown(v: unknown): unknown {
  if (v instanceof Error) {
    return { name: v.name, message: v.message, stack: v.stack };
  }
  return v;
}

function safeStringify(line: Record<string, unknown>): string {
  try {
    return JSON.stringify(line, errorReplacer);
  } catch {
    return JSON.stringify({
      ts: line.ts,
      level: line.level,
      msg: line.msg,
      _stringify_error: true,
    });
  }
}

function errorReplacer(_key: string, value: unknown): unknown {
  if (value instanceof Error) {
    return { name: value.name, message: value.message, stack: value.stack };
  }
  return value;
}
