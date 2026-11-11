import { PlusIcon } from "@/icons";
import { Button } from "@/components/ui/button";
import { TaskFilterBar, FILTER_ROW_TRIGGER_BOX, type TaskFilterBarProps } from "./task-filter-bar";
import { t } from "@/lib/i18n";
import { cn } from "@/lib/cn";
import { useCompactLayout } from "@/navigation/layout";

/** Global and conversation boards share the entire responsive toolbar. Without
 * `onCreate` the row holds filters only (the page puts New task elsewhere). */
export function TaskToolbar({
  onCreate,
  ...filters
}: TaskFilterBarProps & { onCreate?: () => void }) {
  // Wide keeps one line: the filter bar shrinks (its project chips scroll) so
  // New task stays right after it instead of wrapping below.
  const compact = useCompactLayout();

  return (
    <div
      data-task-board-toolbar
      className={cn(
        "flex shrink-0 items-center gap-2 border-b border-border-subtle px-4 py-2",
        compact && "flex-wrap",
      )}
    >
      <TaskFilterBar {...filters} />
      {onCreate ? (
        <Button
          type="button"
          variant="primary"
          data-task-create
          className={`shrink-0 gap-1.5 ${FILTER_ROW_TRIGGER_BOX}`}
          onClick={onCreate}
        >
          <PlusIcon className="size-3.5" />
          {t("tasks.newTask")}
        </Button>
      ) : null}
    </div>
  );
}
