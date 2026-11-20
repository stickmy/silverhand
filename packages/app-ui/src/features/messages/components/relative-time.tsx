import dayjs from "dayjs";
import relativeTime from "dayjs/plugin/relativeTime";
import { memo, useSyncExternalStore } from "react";
import { useLocale } from "@/lib/i18n";

dayjs.extend(relativeTime);

const MINUTE = 60_000;

// One minute clock for every label: a timer per label would wake each of them
// on its own tick, and only the label text has to re-render, never its row.
const clockListeners = new Set<() => void>();
let clockTick = 0;
let clockTimer: ReturnType<typeof setInterval> | null = null;

function subscribeClock(listener: () => void): () => void {
  clockListeners.add(listener);
  clockTimer ??= setInterval(() => {
    clockTick += 1;
    for (const notify of clockListeners) notify();
  }, MINUTE);
  return () => {
    clockListeners.delete(listener);
    if (clockListeners.size === 0 && clockTimer !== null) {
      clearInterval(clockTimer);
      clockTimer = null;
    }
  };
}

const readClock = (): number => clockTick;

/** Full timestamp for a `title`; it does not change with time, so no clock. */
export function absoluteTime(epochMs: number): string {
  return dayjs(epochMs).format("YYYY-MM-DD HH:mm:ss");
}

/** "3 minutes ago", kept current by the shared minute clock. */
export const RelativeTime = memo(function RelativeTime({
  epochMs,
}: {
  epochMs: number;
}): JSX.Element {
  useSyncExternalStore(subscribeClock, readClock, readClock);
  useLocale();
  return <>{dayjs(epochMs).fromNow()}</>;
});
