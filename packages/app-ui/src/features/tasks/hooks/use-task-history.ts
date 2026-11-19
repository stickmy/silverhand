import { useQuery } from "@tanstack/react-query";
import { type TaskSystemMetadata, readTaskSystemMetadata } from "@silverhand/shared";
import { api, toPathSegment } from "@/lib/api";

/** One recorded change to a task, newest first in {@link useTaskHistory}. */
export interface TaskHistoryEntry {
  id: string;
  createdAt: number;
  meta: TaskSystemMetadata;
}

interface TaskHistoryResponse {
  events: Array<{ id: string; created_at: number; metadata: Record<string, unknown> | null }>;
}

export const taskHistoryKey = (taskId: string) => ["task-history", taskId] as const;

/**
 * `GET /api/tasks/:id/history` — the task's lifecycle events (the same task
 * system-message metadata the activity strip renders), gathered by `task_id`
 * across conversations. A server without the route answers 404: no retry, and
 * the section stays hidden.
 */
export function useTaskHistory(taskId: string) {
  return useQuery({
    queryKey: taskHistoryKey(taskId),
    queryFn: async (): Promise<TaskHistoryEntry[]> => {
      const segment = toPathSegment(taskId);
      if (segment === null) return [];
      const res = await api<TaskHistoryResponse>(`/api/tasks/${segment}/history`);
      return res.events
        .flatMap((event) => {
          const meta = readTaskSystemMetadata(event.metadata);
          return meta ? [{ id: event.id, createdAt: event.created_at, meta }] : [];
        })
        .sort((a, b) => b.createdAt - a.createdAt);
    },
    retry: false,
  });
}
