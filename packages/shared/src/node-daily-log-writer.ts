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
const MAX_FILES = 14;

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
    filePath = path;
    nextRotation = (Math.floor(now / DAY_MS) + 1) * DAY_MS;
    try {
      const names = readdirSync(dir);
      // Old boot files can span many days: expire by last write, not their start date.
      const expiredLegacy = names.filter((name) =>
        legacyPrefixes.some((legacy) => {
          const boot =
            name.startsWith(`${legacy}-`) &&
            /^\d{4}-\d{2}-\d{2}T\d{2}-\d{2}-\d{2}-\d{3}Z\.log$/.test(name.slice(legacy.length + 1));
          const renamedDaily =
            legacy !== prefix &&
            name.startsWith(`${legacy}.`) &&
            /^\d{4}-\d{2}-\d{2}\.log$/.test(name.slice(legacy.length + 1));
          return (
            (boot || renamedDaily) && statSync(join(dir, name)).mtimeMs < now - MAX_FILES * DAY_MS
          );
        }),
      );
      const files = names
        .filter(
          (name) =>
            name.startsWith(`${prefix}.`) &&
            /^\d{4}-\d{2}-\d{2}\.log$/.test(name.slice(prefix.length + 1)),
        )
        .sort();
      for (const name of [
        ...expiredLegacy,
        ...files.slice(0, Math.max(0, files.length - MAX_FILES)),
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
          nextRotation = (Math.floor(now / DAY_MS) + 1) * DAY_MS;
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
