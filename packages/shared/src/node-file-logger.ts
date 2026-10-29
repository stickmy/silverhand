import { createDailyLogWriter, type DailyLogWriter } from './node-daily-log-writer.js';
import {
  parseSilverhandLogLevel,
  sanitizeLogFields,
  SILVERHAND_LOG_LEVEL_ENV,
  type SilverhandLogLevel,
} from './logging.js';

type LogEventLevel = SilverhandLogLevel | 'error';

export interface FileLoggerOptions {
  /** Directory to write logs into. One file per UTC day, retaining 14 files per prefix. */
  dir: string;
  /** Also mirror lines to console (default: true in dev, false otherwise). */
  alsoConsole?: boolean;
  /** Static fields included on every line (e.g. process pid). */
  base?: Record<string, unknown>;
  /** Shared mutable level/window state. */
  logControl?: LogLevelControl;
}

export interface FileLogger {
  info(obj: unknown, msg?: string): void;
  warn(obj: unknown, msg?: string): void;
  error(obj: unknown, msg?: string): void;
  /** Absolute path of the log file currently being written. */
  filePath: string;
  /** Mutable level/window state used by this logger and its children. */
  logControl: LogLevelControl;
  debug(obj: unknown, msg?: string): void;
  /** Returns a child logger that merges `bindings` into every line. */
  child(bindings: Record<string, unknown>): FileLogger;
  /** Flush + close the underlying stream. Idempotent. */
  close(): Promise<void>;
}

export interface ConversationDebugStatus {
  enabled: boolean;
  expiresAt: number | null;
}

export interface LogLevelControl {
  getGlobalLevel(): SilverhandLogLevel;
  setGlobalLevel(level: SilverhandLogLevel): void;
  enableConversationDebug(conversationId: string, expiresAt: number): void;
  disableConversationDebug(conversationId: string): void;
  getConversationDebugStatus(conversationId: string): ConversationDebugStatus;
}

/**
 * The desktop main-process log: one JSONL file per UTC day under
 * `<dir>/desktop.YYYY-MM-DD.log`. CLI and agent audit files in the same
 * directory belong to the Rust backend. Format: one JSON object per line —
 * `{ ts, level, msg, ...fields }`.
 *
 * Level is debug|info|warn|error. `SILVERHAND_LOG_LEVEL` accepts
 * error|warn|info|debug and defaults to warn. Per-conversation debug
 * windows are held in the shared `LogLevelControl`; a line must carry
 * `conversationId` to be lifted by that window.
 */
export function createFileLogger(opts: FileLoggerOptions): FileLogger {
  // Retired desktop names: `backend` (main process) and `agent` (an unused sink).
  const writer = createDailyLogWriter(opts.dir, 'desktop', ['backend', 'agent']);
  const alsoConsole = opts.alsoConsole ?? true;
  const base: Record<string, unknown> = { pid: process.pid, ...(opts.base ?? {}) };
  const logControl = opts.logControl ?? createLogLevelControl();

  return makeLogger(writer, alsoConsole, base, logControl, ALWAYS_WRITE_CATEGORIES);
}

// Owner-audit read faces (#task:2036 part ②): the OWNER reading an agent's
// third-party conversations is an authority-sensitive access, and in the v1
// single-host model the system logger IS the audit trail — there is no separate
// audit store. So these three records (chats listed, transcript read, attachment
// bytes read) must survive the DEFAULT `warn` level (an `info` line is otherwise
// dropped): they name a category the system sink writes unconditionally, exactly
// regardless of level.
const AGENT_AUDIT_CATEGORIES = [
  'agent_audit.chats_listed',
  'agent_audit.transcript_read',
  'agent_audit.attachment_read',
] as const;
const EVENT_WS_DIAGNOSTIC_CATEGORIES = [
  'event_ws.publish',
  'event_ws.subscribe',
  'event_ws.unsubscribe',
  'event_ws.resume',
  'event_ws.snapshot_required',
  'event_ws.send_skipped_non_open',
  'event_ws.renderer',
] as const;
// Records whose value is in being complete, not in clearing a severity threshold.
const ALWAYS_WRITE_CATEGORIES: ReadonlySet<string> = new Set([
  ...EVENT_WS_DIAGNOSTIC_CATEGORIES,
  ...AGENT_AUDIT_CATEGORIES,
]);

export function createLogLevelControl(
  initialLevel: SilverhandLogLevel = parseSilverhandLogLevel(process.env[SILVERHAND_LOG_LEVEL_ENV]),
): LogLevelControl {
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

function makeLogger(
  writer: DailyLogWriter,
  alsoConsole: boolean,
  base: Record<string, unknown>,
  logControl: LogLevelControl,
  alwaysWriteCategories: ReadonlySet<string> = new Set(),
): FileLogger {
  const safeBase = sanitizeLogFields(base);
  const write = (level: LogEventLevel, obj: unknown, msg?: string) => {
    const rawFields =
      obj && typeof obj === 'object' && !(obj instanceof Error)
        ? (obj as Record<string, unknown>)
        : { data: serializeUnknown(obj) };
    const fields = sanitizeLogFields(rawFields);
    const now = new Date();
    const line = {
      ts: now.toISOString(),
      level,
      msg: msg ?? '',
      ...safeBase,
      ...fields,
    };
    if (!shouldWrite(level, line, logControl, alwaysWriteCategories)) return;
    writer.write(safeStringify(line) + '\n', now.getTime());
    if (alsoConsole) {
      const fn = level === 'error' ? console.error : level === 'warn' ? console.warn : console.log;
      fn(`[${level}] ${msg ?? ''}`, fields);
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
    child: (bindings) =>
      makeLogger(
        writer,
        alsoConsole,
        { ...safeBase, ...bindings },
        logControl,
        alwaysWriteCategories,
      ),
    close: () => writer.close(),
  };
}

function shouldWrite(
  eventLevel: LogEventLevel,
  line: Record<string, unknown>,
  logControl: LogLevelControl,
  alwaysWriteCategories: ReadonlySet<string>,
): boolean {
  if (eventLevel === 'error') return true;
  const category = typeof line.category === 'string' ? line.category : null;
  if (category && alwaysWriteCategories.has(category)) return true;
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
