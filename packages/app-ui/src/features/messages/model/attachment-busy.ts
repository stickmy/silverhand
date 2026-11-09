import { useSyncExternalStore } from "react";

// Saving and opening an attachment both fetch every byte before the system
// sheet or app can take over, which on a large file or a weak network takes
// seconds. Controls read this to show the wait from the tap; a repeat tap
// joins the run already under way.

export type AttachmentTask = "save" | "open";

const running = new Set<string>();
const listeners = new Set<() => void>();

const keyOf = (task: AttachmentTask, id: string) => `${task}:${id}`;

function notify(): void {
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** Runs `work` unless the same task on the same attachment is already
 *  running, and marks it busy until `work` settles. */
export async function runAttachmentTask(
  task: AttachmentTask,
  id: string,
  work: () => Promise<void>,
): Promise<void> {
  const key = keyOf(task, id);
  if (running.has(key)) return;
  running.add(key);
  notify();
  try {
    await work();
  } finally {
    running.delete(key);
    notify();
  }
}

export function useAttachmentTaskRunning(task: AttachmentTask, id: string): boolean {
  return useSyncExternalStore(subscribe, () => running.has(keyOf(task, id)));
}
