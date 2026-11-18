/** One presentation priority for rendering, read cursors and notification focus.
 * Lower-priority URL state can remain present as a return destination. */
export function selectSecondarySurface(input: {
  agent: boolean;
  human: boolean;
  settings: boolean;
  files: boolean;
  attachment: boolean;
  task: boolean;
  conversation: boolean;
  thread: boolean;
  threads: boolean;
}) {
  for (const kind of [
    "agent",
    "human",
    "settings",
    "files",
    "attachment",
    "task",
    "conversation",
    "thread",
    "threads",
  ] as const) {
    if (input[kind]) return kind;
  }
  return "none";
}
