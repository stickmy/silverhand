import {
  createWriteStream,
  mkdirSync,
  openSync,
  readdirSync,
  statSync,
  unlinkSync,
  type WriteStream,
} from 'node:fs';
import { join } from 'node:path';

const DAY_MS = 86_400_000;
/** Daily files kept per prefix; also the idle age, in days, after which a legacy file expires. */
const RETAIN_DAYS = 14;
const DAILY_STAMP = /^\d{4}-\d{2}-\d{2}\.log$/;
const BOOT_STAMP = /^\d{4}-\d{2}-\d{2}T\d{2}-\d{2}-\d{2}-\d{3}Z\.log$/;

const stampAfter = (name: string, prefix: string, separator: string): string | null =>
  name.startsWith(prefix + separator) ? name.slice(prefix.length + 1) : null;
const isDaily = (name: string, prefix: string) =>
  DAILY_STAMP.test(stampAfter(name, prefix, '.') ?? '');
const isBoot = (name: string, prefix: string) =>
  BOOT_STAMP.test(stampAfter(name, prefix, '-') ?? '');

const nextBoundary = (now: number) => (Math.floor(now / DAY_MS) + 1) * DAY_MS;

function idleSince(path: string, cutoff: number): boolean {
  try {
    return statSync(path).mtimeMs < cutoff;
  } catch {
    // Removed or unreadable mid-scan: skip it rather than abort the pass.
    return false;
  }
}

/** Process-owned UTC daily files. No timer: a write past midnight rolls and prunes. */
export function createDailyLogWriter(
  dir: string,
  prefix: string,
  legacyPrefixes: readonly string[] = [],
) {
  mkdirSync(dir, { recursive: true });
  let filePath: string;
  let nextRotation: number;
  const closing = new Set<Promise<void>>();
  let closed: Promise<void> | undefined;

  function open(now: number): WriteStream {
    const date = new Date(now).toISOString().slice(0, 10);
    const filename = `${prefix}.${date}.log`;
    const path = join(dir, filename);
    // Create before pruning so even rapid successive rollovers count every file.
    const stream = createWriteStream(path, { fd: openSync(path, 'a') });
    // An I/O fault (e.g. ENOSPC) must cost log lines, never crash the process.
    stream.on('error', (error) => console.error('Log write failed', path, error));
    filePath = path;
    nextRotation = nextBoundary(now);
    try {
      const names = readdirSync(dir);
      // Old boot files can span many days: expire by last write, not their start date.
      const cutoff = now - RETAIN_DAYS * DAY_MS;
      const expiredLegacy = names.filter(
        (name) =>
          legacyPrefixes.some(
            (legacy) => isBoot(name, legacy) || (legacy !== prefix && isDaily(name, legacy)),
          ) && idleSince(join(dir, name), cutoff),
      );
      const files = names.filter((name) => isDaily(name, prefix)).sort();
      for (const name of [
        ...expiredLegacy,
        ...files.slice(0, Math.max(0, files.length - RETAIN_DAYS)),
      ]) {
        if (name === filename) continue;
        try {
          unlinkSync(join(dir, name));
        } catch (error) {
          // Another instance may have pruned the same daily file already.
          if ((error as NodeJS.ErrnoException).code !== 'ENOENT') {
            console.error('Failed to remove old log file', name, error);
          }
        }
      }
    } catch (error) {
      console.error('Failed to list old log files', error);
    }
    return stream;
  }

  function finish(stream: WriteStream): Promise<void> {
    const done = new Promise<void>((resolve) => stream.end(resolve));
    closing.add(done);
    void done.then(() => closing.delete(done));
    return done;
  }

  let stream = open(Date.now());
  return {
    get filePath(): string {
      return filePath;
    },
    write(line: string, now: number): void {
      if (closed) return;
      if (now >= nextRotation) {
        try {
          const next = open(now);
          finish(stream);
          stream = next;
        } catch (error) {
          // Like the Rust appender, keep the current file if the new one cannot open.
          nextRotation = nextBoundary(now);
          console.error('Failed to rotate log file', error);
        }
      }
      stream.write(line);
    },
    close(): Promise<void> {
      if (!closed) {
        finish(stream);
        closed = Promise.all([...closing]).then(() => undefined);
      }
      return closed;
    },
  };
}

export type DailyLogWriter = ReturnType<typeof createDailyLogWriter>;
