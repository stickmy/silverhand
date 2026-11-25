import type { TaskDetailDto, TaskStatus } from "@silverhand/shared";
import {
  useTaskMetaRows,
  type TaskMetaWriteProps,
} from "@/features/tasks/components/task-detail-shared";
import type { TaskAssigneeRef } from "@/features/tasks/hooks/hooks";
import { t } from "@/lib/i18n";

export interface TaskDetailMetaPanelProps {
  view: TaskDetailDto;
  busy: boolean;
  statusPending: boolean;
  statusFailed: boolean;
  onStatusChange: (status: TaskStatus) => void;
  assignPending: boolean;
  assignFailed: boolean;
  /** `null` clears the assignee. */
  onAssign: (assignee: TaskAssigneeRef | null) => void;
}

export function TaskDetailMetaPanel(props: TaskDetailMetaPanelProps): JSX.Element {
  const rows = useTaskMetaRows(props satisfies TaskMetaWriteProps);
  return (
    <aside
      data-task-meta-panel
      aria-label={t("taskPage.taskMetadata")}
      className="flex w-60 shrink-0 flex-col gap-y-3.5 overflow-y-auto border-l border-border-subtle px-4 py-4"
    >
      {rows.map((row) => (
        <MetaRow key={row.key} label={row.label} error={row.error}>
          {row.value}
        </MetaRow>
      ))}
    </aside>
  );
}

function MetaRow({
  label,
  error,
  children,
}: {
  label: string;
  error?: string | null;
  children: React.ReactNode;
}): JSX.Element {
  return (
    <div className="grid grid-cols-[68px_1fr] items-start gap-x-2">
      <span className="chrome-label text-(length:--text-badge) font-medium leading-4 text-text-muted">
        {label}
      </span>
      <div className="min-w-0 text-xs leading-4 text-text-primary">
        {children}
        {error ? <p className="mt-0.5 text-(length:--text-meta) text-error">{error}</p> : null}
      </div>
    </div>
  );
}
