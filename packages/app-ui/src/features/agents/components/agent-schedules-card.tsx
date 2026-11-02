import { SchedulesSkeleton } from "./agent-loading";
import { useEffect, useMemo, useState } from "react";
import {
  CancelledIcon,
  ClockIcon,
  ReloadIcon,
  RepeatIcon,
  ScheduledIcon,
  TimerIcon,
} from "@/icons";
import dayjs from "dayjs";
import relativeTime from "dayjs/plugin/relativeTime";
import type { ScheduleDto, ScheduleListStatusFilter } from "@silverhand/shared";
import { toast } from "sonner";
import { useNav } from "@/router";
import { useConversationIdentity } from "@/features/conversations/conversation-identity";
import { cn } from "@/lib/cn";
import { useAgentSchedules, useCancelAgentSchedule } from "../hooks/hooks";
import { t } from "@/lib/i18n";

import "./agent-schedules-card.css";

dayjs.extend(relativeTime);

interface AgentSchedulesCardProps {
  agentId: string;
}

const statusFilters = (): Array<{ value: ScheduleListStatusFilter; label: string }> => [
  { value: "active", label: t("agents.active") },
  { value: "all", label: t("agents.all") },
  { value: "done", label: t("agents.done") },
  { value: "cancelled", label: t("agents.cancelled2") },
  { value: "failed", label: t("agents.failed2") },
];

/** Creation is agent-only; this surface exposes schedule status and cancellation. */
export function AgentSchedulesCard({ agentId }: AgentSchedulesCardProps): JSX.Element {
  const [filter, setFilter] = useState<ScheduleListStatusFilter>("active");
  const query = useAgentSchedules(agentId, filter);
  const cancel = useCancelAgentSchedule(agentId);

  const rows = useMemo(() => query.data ?? [], [query.data]);

  return (
    <div className="agent-schedules flex min-w-0 flex-col gap-3">
      {/* Toolbar: filter + refresh, left-aligned. The "Self-scheduled wakes…"
          description was dropped (shiki) — the card's read-only/cancel-only
          nature is conveyed by the rows themselves, not a caption. */}
      <header className="flex items-stretch gap-1">
        <FilterTabs value={filter} onChange={setFilter} />
        <button
          type="button"
          onClick={() => query.refetch()}
          disabled={query.isFetching}
          data-panel-control="row-action"
          className="inline-flex min-w-7 shrink-0 items-center justify-center px-1.5 text-(length:--text-meta) text-text-muted transition-colors hover:bg-bg-hover hover:text-text-primary disabled:opacity-50"
          aria-label={t("agents.refreshSchedules")}
          title={t("agents.refresh")}
        >
          <ReloadIcon className={`size-3 ${query.isFetching ? "animate-spin" : ""}`} />
        </button>
      </header>

      {query.isError ? (
        <div
          data-panel-card="placeholder"
          className="border border-dashed border-error/40 bg-error/5 px-3 py-2 text-xs text-error"
        >
          {t("agents.failedToLoadSchedules")}
        </div>
      ) : null}

      {query.isLoading ? (
        <SchedulesSkeleton />
      ) : rows.length === 0 ? (
        <div
          data-panel-card="placeholder"
          className="border border-dashed border-border-subtle px-4 py-6 text-center text-xs text-text-muted"
        >
          {filter === "active"
            ? t("agents.noActiveSchedules")
            : t("agents.noSchedulesWithStatus", {
                status: statusFilters().find((opt) => opt.value === filter)?.label ?? filter,
              })}
        </div>
      ) : (
        <ul className="flex flex-col gap-2">
          {rows.map((row) => (
            <ScheduleItem
              key={row.id}
              row={row}
              cancelling={cancel.isPending && cancel.variables === row.id}
              onCancel={() => cancel.mutate(row.id)}
            />
          ))}
        </ul>
      )}
    </div>
  );
}

function ScheduleItem({
  row,
  cancelling,
  onCancel,
}: {
  row: ScheduleDto;
  cancelling: boolean;
  onCancel: () => void;
}): JSX.Element {
  const cancellable = row.status === "pending" || row.status === "firing";

  return (
    <li
      data-panel-card="schedule"
      className={cn(
        "schedule-card border border-border-subtle bg-bg-floating/50 transition-colors hover:border-border",
        !cancellable && "opacity-70",
      )}
    >
      <div className="contents">
        <div
          className="schedule-title truncate text-sm font-medium text-text-primary"
          title={row.title}
        >
          {row.title}
        </div>
        <div className="schedule-metadata text-(length:--text-meta)">
          <span
            className="inline-flex h-5 shrink-0 items-center gap-1 font-medium leading-5 text-text-secondary"
            title={cancellable ? dayjs(row.next_fire_at).toISOString() : undefined}
            aria-label={cancellable ? t("agents.nextTrigger") : t("agents.scheduleStatus")}
          >
            {cancellable ? <ClockIcon className="size-3 shrink-0" aria-hidden="true" /> : null}
            {cancellable ? dayjs(row.next_fire_at).fromNow() : row.status}
          </span>
          <ScheduleTiming row={row} />
          <ScheduleTarget label={row.target_label} conversationId={row.target_conversation_id} />
        </div>
      </div>
      {cancellable ? (
        <button
          type="button"
          onClick={onCancel}
          disabled={cancelling}
          data-panel-control="row-action"
          className="schedule-cancel inline-flex size-7 shrink-0 items-center justify-center text-text-muted transition-all duration-150 hover:scale-110 hover:text-text-primary focus-outline disabled:opacity-50 disabled:hover:scale-100"
          aria-label={t("agents.cancelSchedule")}
          title={t("common.cancel")}
        >
          <CancelledIcon className="size-3.5" />
        </button>
      ) : null}
    </li>
  );
}

function ScheduleTiming({ row }: { row: ScheduleDto }): JSX.Element {
  const spec = row.spec;
  const Icon =
    spec.kind === "once" ? ScheduledIcon : spec.kind === "interval" ? TimerIcon : RepeatIcon;
  const timing =
    spec.kind === "once"
      ? dayjs(spec.fire_at).format("YYYY-MM-DD HH:mm")
      : spec.kind === "interval"
        ? `every ${spec.every_seconds}s`
        : spec.expression;

  return (
    <span
      className="schedule-timing inline-flex min-w-0 max-w-full items-center gap-1.5 rounded-(--chip-radius) bg-(--chip-bg) px-2 py-0.5 text-text-secondary"
      title={[spec.kind, timing, spec.kind === "cron" ? spec.timezone : null, formatFiredCount(row)]
        .filter(Boolean)
        .join("\n")}
    >
      <Icon className="size-3 shrink-0" aria-hidden="true" />
      <span className="shrink-0 font-medium">{spec.kind}</span>
      <span className="truncate font-mono">{timing}</span>
      {spec.kind === "cron" ? (
        <span className="schedule-timezone min-w-0 text-text-muted">{spec.timezone}</span>
      ) : null}
    </span>
  );
}

function ScheduleTarget({
  label,
  conversationId,
}: {
  label: string;
  conversationId: string;
}): JSX.Element {
  const [requested, setRequested] = useState(false);
  const resolution = useConversationIdentity(requested ? conversationId : null);
  const { toChannel, toDirect } = useNav();

  // The schedule label does not grant access to its target. Resolve only on click.
  useEffect(() => {
    if (!requested || resolution.status === "loading") {
      return;
    }

    setRequested(false);
    if (resolution.status !== "resolved") {
      toast.error(t("agents.conversationUnavailable"));
      return;
    }

    const identity = resolution.identity;
    if (identity.placement === "workspace") {
      toChannel(identity.ref.workspace_id, identity.ref.id);
    } else {
      toDirect(identity.pair.conversation_id);
    }
  }, [requested, resolution, toChannel, toDirect]);

  return (
    <button
      type="button"
      className="min-w-0 max-w-full truncate text-text-secondary underline-offset-2 hover:text-text-primary hover:underline focus-outline disabled:opacity-50"
      onClick={() => setRequested(true)}
      disabled={requested}
      title={label}
      aria-label={t("agents.openTarget", { target: label })}
    >
      {label}
    </button>
  );
}

export function formatFiredCount(row: ScheduleDto): string {
  const fired = row.fired_count;
  if (row.spec.kind === "once") {
    // A `once` row's capacity is always 1, even when the DTO leaves
    // `max_occurrences` null.
    return `${fired} / 1 fired`;
  }
  if (row.max_occurrences != null) {
    return `${fired} / ${row.max_occurrences} fired`;
  }
  return `${fired} / ∞ fired`;
}

function FilterTabs({
  value,
  onChange,
}: {
  value: ScheduleListStatusFilter;
  onChange: (next: ScheduleListStatusFilter) => void;
}): JSX.Element {
  return (
    <div
      data-panel-tabs
      className="scrollbar-hide flex min-w-0 items-center overflow-x-auto border border-border-subtle bg-bg-base p-0.5"
    >
      {statusFilters().map((opt) => (
        <button
          key={opt.value}
          type="button"
          onClick={() => onChange(opt.value)}
          className={cn(
            "shrink-0 whitespace-nowrap px-2 py-0.5 text-(length:--text-meta) transition-colors",
            opt.value === value
              ? "bg-accent text-text-on-accent"
              : "text-text-muted hover:text-text-primary",
          )}
        >
          {opt.label}
        </button>
      ))}
    </div>
  );
}
